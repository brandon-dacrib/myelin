//! History placed in the timeline after the fact, by either kind of backfill: the room's history
//! from before the oldest event held ([`HistoryKind::BeforeOldest`], a room joined elsewhere) and
//! the history between a leave and a rejoin ([`HistoryKind::Gap`], `super::gaps`). Both arrive
//! the same way -- a `/backfill` batch from another server, verified by the caller -- and both
//! go through [`RoomActor::accept_history`]: select what belongs, work out the state at each
//! event, authorize each event at its position, place what passes.
//!
//! # The state at each event
//! The exact way, when the caller could ask the server that sent the batch
//! ([`FetchedState`]): the state *before the oldest event* of the batch, as that server answers
//! `/state_ids`, with any state or auth event this server did not hold fetched and stored as an
//! outlier ([`RoomActor::store_fetched_state_events`]); the state before every later event is
//! derived forward from it, applying each accepted state event of the batch in topological
//! order. A key set long before the batch (a topic, a member who joined years ago) is there at
//! every event.
//!
//! The fallback, when nothing was fetched (the server could not answer; a caller without
//! federation): walked back from the state held at the event the batch sits below, reverting
//! each state event passed to its predecessor in the batch -- exact while the history is linear
//! and every reverted key's predecessor is in the batch, and otherwise a key reads as unset (or,
//! in a gap, as it was at the leave) before its oldest setting in the batch.
//!
//! # Authorization
//! Every event is authorized as an inbound one is ([`RoomActor::accept_remote_event`]), with
//! `hs_state::auth`: its `auth_events` must be the right selection
//! ([`auth::check_auth_events_selection`]) and allow it ([`auth::check_event_auth`]), and, when
//! the state was fetched, so must the state before it. Its `auth_events` are resolved from what
//! is held, what the fetch brought, and the batch's own earlier accepted events. An event that
//! fails is **not placed** -- not stored at all, as an inbound one is refused -- and is counted
//! as rejected; the state derived for later events does not include it. When the state was
//! walked, a check against the walked state would reject on its known inexactness, so only the
//! `auth_events` checks run, and an event whose `auth_events` are out of reach is placed
//! unchecked (counted) as before.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use hs_kv::KvBackend;
use hs_model::Event;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue};
use hs_model::ids::EventSn;
use hs_state::auth::{self, AuthEventRef, IncomingEvent};
use hs_state::error::AuthError;
use hs_state::state_fetch::{FlatState, StateEntry, StateFetch};
use ruma::{EventId, OwnedEventId, UserId};

use super::gaps::Gap;
use super::{PlannedHistory, RoomActor, extract_redacts, topological_order};
use crate::backfill::{FetchedState, HistoryKind, HistoryOutcome, HistoryPlan, StateSource};
use crate::error::RoomError;
use crate::pipeline;

/// A room state as `(type, state_key) -> event ID`.
pub(super) type StateMap = BTreeMap<(String, String), OwnedEventId>;

/// The content every event without one is read as.
static EMPTY_CONTENT: CanonicalJsonObject = CanonicalJsonObject::new();

/// An event's `content`, or the empty object.
fn content_of(event: &Event) -> &CanonicalJsonObject {
    event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .unwrap_or(&EMPTY_CONTENT)
}

/// An event's `(type, state_key)`, if it is a state event.
fn state_key_of(event: &Event) -> Option<(String, String)> {
    event
        .header()
        .state_key
        .clone()
        .map(|state_key| (event.header().event_type.clone(), state_key))
}

/// A batch, selected and ordered, and where it goes.
pub(super) struct Selected {
    /// What will be placed (unless it fails authorization), oldest first.
    batch: Vec<Event>,
    /// The timeline event the batch sits below: the state walk starts from the state before it.
    anchor_sn: EventSn,
    /// The position the newest placed event gets; each older one goes one lower.
    newest_pos: i64,
    /// The most events there is room for (a gap's positions are finite).
    room_left: Option<usize>,
    /// What a walk falls back to for a key the batch holds no earlier setting of.
    walk_fallback: StateMap,
    /// For a gap: which, and the events too old to be part of it.
    gap: Option<GapSelection>,
}

/// The gap part of [`Selected`].
pub(super) struct GapSelection {
    pub(super) top: i64,
    pub(super) gap: Gap,
    /// Events of the answer no deeper than the gap's `below`: history from before the leave.
    pub(super) older_than_gap: HashSet<OwnedEventId>,
}

/// What [`RoomActor::select_history`] found.
pub(super) enum Selection {
    /// The gap is closed: nothing goes in it.
    Closed,
    /// A batch (possibly empty).
    Batch(Selected),
}

/// The verdict on one event of a batch.
enum Verdict {
    Allowed,
    /// Placed without a check: its `auth_events` are out of reach and the state was walked.
    Unchecked,
    Rejected(String),
}

/// Looks events up by ID among what the actor holds and a batch's accepted events.
struct Lookup<'a, B: KvBackend> {
    actor: &'a RoomActor<B>,
    local: &'a HashMap<OwnedEventId, Event>,
}

impl<'a, B: KvBackend> Lookup<'a, B> {
    fn event(&self, id: &EventId) -> Option<&'a Event> {
        self.actor
            .event_id_index
            .get(id)
            .and_then(|sn| self.actor.events.get(sn))
            .or_else(|| self.local.get(id))
    }
}

/// A [`StateFetch`] over a [`StateMap`], reading bodies through a [`Lookup`].
struct MapStateFetch<'a, B: KvBackend> {
    map: &'a StateMap,
    lookup: &'a Lookup<'a, B>,
}

impl<B: KvBackend> StateFetch for MapStateFetch<'_, B> {
    fn get(&self, event_type: &str, state_key: &str) -> Option<StateEntry<'_>> {
        let id = self
            .map
            .get(&(event_type.to_owned(), state_key.to_owned()))?;
        let event = self.lookup.event(id)?;
        Some(StateEntry {
            sender: AsRef::<UserId>::as_ref(&event.header().sender),
            content: content_of(event),
        })
    }
}

impl<B: KvBackend> RoomActor<B> {
    /// Selects the events of `events` that belong in the history `kind` names, oldest first.
    /// Skipped: duplicates, events for another room, events already in the timeline, and events
    /// newer than the event the batch sits below; for a gap, also events no deeper than the gap's
    /// `below` (history from before the leave).
    ///
    /// # Errors
    /// [`RoomError::Internal`] for a room with no timeline, or no gap below `top`;
    /// [`RoomError::State`] or [`RoomError::Store`] reading the state a walk would fall back to.
    pub(super) fn select_history(
        &self,
        kind: HistoryKind,
        events: Vec<Event>,
    ) -> Result<Selection, RoomError> {
        let (anchor_pos, anchor_sn, max_depth, min_depth, gap) = match kind {
            HistoryKind::BeforeOldest => {
                let Some((&pos, &sn)) = self.timeline.iter().next() else {
                    return Err(RoomError::Internal(
                        "a room with no timeline cannot be backfilled".into(),
                    ));
                };
                let depth = self
                    .events
                    .get(&sn)
                    .ok_or_else(|| {
                        RoomError::Internal("oldest timeline event not in hot cache".into())
                    })?
                    .header()
                    .depth;
                (pos, sn, depth, None, None)
            }
            HistoryKind::Gap { top } => {
                let Some(gap) = self.gaps.get(&top).cloned() else {
                    return Err(RoomError::Internal(format!(
                        "no timeline gap below position {top} in {}",
                        self.room_id
                    )));
                };
                if gap.closed {
                    return Ok(Selection::Closed);
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
                let anchor_sn = *self.timeline.get(&gap.filled_to).ok_or_else(|| {
                    RoomError::Internal("a timeline gap's lowest filled event".into())
                })?;
                (
                    gap.filled_to,
                    anchor_sn,
                    top_depth,
                    Some(below_depth),
                    Some((top, gap)),
                )
            }
        };

        let in_timeline: HashSet<EventSn> = self.timeline.values().copied().collect();
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
                    kind = kind.label(),
                    "dropping a backfilled event that is for another room"
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
            if depth > max_depth {
                tracing::warn!(
                    room_id = %self.room_id,
                    event_id = %event.event_id(),
                    depth,
                    max_depth,
                    kind = kind.label(),
                    "dropping a backfilled event newer than the event the batch sits below"
                );
                continue;
            }
            if min_depth.is_some_and(|below| depth <= below) {
                older_than_gap.insert(event.event_id().to_owned());
                continue;
            }
            batch.push(event);
        }
        batch.sort_by(topological_order); // oldest first

        let (newest_pos, room_left, walk_fallback, gap) = match gap {
            None => (anchor_pos.min(0) - 1, None, StateMap::new(), None),
            Some((top, gap)) => {
                let room_left = usize::try_from(gap.filled_to - gap.below - 1).unwrap_or(0);
                let fallback = match self.timeline.get(&gap.below) {
                    Some(&below_sn) => self.state_map_after(below_sn)?,
                    None => StateMap::new(),
                };
                (
                    gap.filled_to - 1,
                    Some(room_left),
                    fallback,
                    Some(GapSelection {
                        top,
                        gap,
                        older_than_gap,
                    }),
                )
            }
        };
        Ok(Selection::Batch(Selected {
            batch,
            anchor_sn,
            newest_pos,
            room_left,
            walk_fallback,
            gap,
        }))
    }

    /// What fetching the state for a batch of `kind` history needs before it is placed: the
    /// oldest event that would be placed (`/state_ids` is asked at it) and the `auth_events` the
    /// batch cites that are neither held nor in it. `None` when nothing of `events` would be
    /// placed (or the gap is closed). Read-only: `crate::backfill`'s implementation calls this,
    /// fetches, then hands the batch and what it fetched to [`RoomActor::accept_history`].
    ///
    /// # Errors
    /// As [`RoomActor::accept_history`]'s selection: [`RoomError::Internal`] for a room with no
    /// timeline or no gap below `top`; [`RoomError::State`] or [`RoomError::Store`].
    pub fn plan_history(
        &self,
        kind: HistoryKind,
        events: &[Event],
    ) -> Result<Option<HistoryPlan>, RoomError> {
        let Selection::Batch(selected) = self.select_history(kind, events.to_vec())? else {
            return Ok(None);
        };
        let Some(oldest) = selected.batch.first() else {
            return Ok(None);
        };
        let in_batch: HashSet<&EventId> = selected.batch.iter().map(Event::event_id).collect();
        let mut missing_auth: BTreeSet<OwnedEventId> = BTreeSet::new();
        for event in &selected.batch {
            for id in pipeline::decode_event_ids(event.json().get("auth_events")) {
                if !self.event_id_index.contains_key(&id) && !in_batch.contains(&*id) {
                    missing_auth.insert(id);
                }
            }
        }
        Ok(Some(HistoryPlan {
            oldest: oldest.event_id().to_owned(),
            missing_auth: missing_auth.into_iter().collect(),
        }))
    }

    /// The IDs of `ids` this actor does not hold (in the timeline or as outliers): what a state
    /// fetch still has to ask for.
    #[must_use]
    pub fn events_not_held(&self, ids: &[OwnedEventId]) -> Vec<OwnedEventId> {
        let mut seen = HashSet::new();
        ids.iter()
            .filter(|id| !self.event_id_index.contains_key(*id) && seen.insert(*id))
            .cloned()
            .collect()
    }

    /// Stores one batch of history -- `kind` says which -- as fetched from another server by
    /// `crate::backfill` and verified by its caller (hashes and signatures; nothing here checks
    /// either). See the module docs for how the state at each event is worked out and how each
    /// event is authorized; [`RoomActor::accept_backfilled_events`] and
    /// [`RoomActor::accept_gap_events`] say where each kind's events go.
    ///
    /// `fetched` is the state at the batch's oldest event as the sending server answered it
    /// (see [`RoomActor::plan_history`]); `None`, or a `fetched` asked at another event, walks
    /// instead.
    ///
    /// # What does not happen
    /// No [`crate::protocol::RoomUpdate`] is published, and neither the forward extremities nor
    /// the joined-rooms index change: this is history, not news.
    ///
    /// # Errors
    /// [`RoomError::Internal`] for a room with no timeline, or no gap below `top`;
    /// [`RoomError::Store`], [`RoomError::Fenced`] or [`RoomError::State`] from persistence.
    pub fn accept_history(
        &mut self,
        kind: HistoryKind,
        events: Vec<Event>,
        fetched: Option<FetchedState>,
    ) -> Result<HistoryOutcome, RoomError> {
        let mut outcome = HistoryOutcome {
            added: 0,
            rejected: 0,
            unchecked: 0,
            state_events_stored: 0,
            state: StateSource::Walked,
            gap_closed: false,
        };
        let selected = match self.select_history(kind, events)? {
            Selection::Closed => {
                outcome.gap_closed = true;
                return Ok(outcome);
            }
            Selection::Batch(selected) => selected,
        };
        let Selected {
            batch,
            anchor_sn,
            newest_pos,
            room_left,
            walk_fallback,
            gap,
        } = selected;
        let Some(oldest) = batch.first().map(|e| e.event_id().to_owned()) else {
            if let Some(gap) = gap {
                self.close_gap(gap.top)?;
                tracing::debug!(room_id = %self.room_id, top = gap.top, "a timeline gap is closed: the answer held nothing new");
                outcome.gap_closed = true;
            }
            return Ok(outcome);
        };

        // The state: fetched and derived forward, or walked back.
        let fetched = match fetched {
            Some(fetched) if fetched.at == oldest => Some(fetched),
            Some(fetched) => {
                tracing::warn!(
                    room_id = %self.room_id,
                    asked_at = %fetched.at,
                    %oldest,
                    kind = kind.label(),
                    "the state was fetched at another event than the batch's oldest; walking instead"
                );
                None
            }
            None => None,
        };
        let mut derived: Option<StateMap> = None;
        let mut walked: Vec<StateMap> = Vec::new();
        match fetched {
            Some(fetched) => {
                outcome.state_events_stored = self.store_fetched_state_events(fetched.events)?;
                derived = Some(self.state_map_from_ids(&fetched.state_ids));
                outcome.state = StateSource::Fetched;
            }
            None => walked = self.walk_history_states(anchor_sn, &batch, &walk_fallback)?,
        }

        // Authorize, oldest first, deriving the state forward as each event is accepted.
        let create_held = self.state_event("m.room.create", "")?.is_some();
        let mut accepted: HashMap<OwnedEventId, Event> = HashMap::with_capacity(batch.len());
        let mut order: Vec<(OwnedEventId, StateMap)> = Vec::with_capacity(batch.len());
        let mut walked = walked.into_iter();
        for event in batch {
            let state_before = match &derived {
                Some(state) => state.clone(),
                None => walked.next().unwrap_or_default(),
            };
            let verdict = {
                let lookup = Lookup {
                    actor: self,
                    local: &accepted,
                };
                let state = derived.as_ref().map(|_| MapStateFetch {
                    map: &state_before,
                    lookup: &lookup,
                });
                self.authorize_history_event(
                    &event,
                    &lookup,
                    state.as_ref(),
                    derived.is_some(),
                    create_held || state_before.contains_key(&("m.room.create".into(), "".into())),
                )
            };
            match verdict {
                Verdict::Allowed => {}
                Verdict::Unchecked => outcome.unchecked += 1,
                Verdict::Rejected(reason) => {
                    outcome.rejected += 1;
                    tracing::warn!(
                        room_id = %self.room_id,
                        event_id = %event.event_id(),
                        sender = %event.header().sender,
                        event_type = %event.header().event_type,
                        kind = kind.label(),
                        reason,
                        "a backfilled event is not authorized at its position; it is not placed"
                    );
                    self.rejected_history.insert(event.event_id().to_owned());
                    continue;
                }
            }
            if let (Some(state), Some(key)) = (derived.as_mut(), state_key_of(&event)) {
                state.insert(key, event.event_id().to_owned());
            }
            order.push((event.event_id().to_owned(), state_before));
            accepted.insert(event.event_id().to_owned(), event);
        }

        // A gap has only so many positions: the newest that fit are placed.
        let exhausted = room_left.is_some_and(|room| order.len() > room);
        if exhausted {
            let room = room_left.unwrap_or(0);
            tracing::warn!(
                room_id = %self.room_id,
                fetched = order.len(),
                room_left = room,
                kind = kind.label(),
                "a timeline gap has run out of positions; the oldest of its history is left on the other server"
            );
            order.drain(..order.len() - room);
        }

        let mut cited: BTreeSet<OwnedEventId> = BTreeSet::new();
        let mut planned: Vec<PlannedHistory> = Vec::with_capacity(order.len());
        for ((id, state_before), i) in order.into_iter().rev().zip(0i64..) {
            let Some(event) = accepted.remove(&id) else {
                continue;
            };
            cited.extend(pipeline::decode_event_ids(event.json().get("prev_events")));
            planned.push(PlannedHistory {
                held_as: self.event_id_index.get(event.event_id()).copied(),
                event,
                room_pos: newest_pos - i,
                state_before: state_before.into_values().collect(),
            });
        }
        let placed = planned.len();
        outcome.added = self.place_history(planned)?;
        if let Some(gap) = gap {
            outcome.gap_closed = self.finish_gap_fill(gap, cited, placed, exhausted)?;
        }
        tracing::debug!(
            room_id = %self.room_id,
            kind = kind.label(),
            added = outcome.added,
            rejected = outcome.rejected,
            unchecked = outcome.unchecked,
            state = outcome.state.label(),
            "placed backfilled history in the timeline"
        );
        Ok(outcome)
    }

    /// Authorizes one event of a history batch (or one event a state fetch brought, with no
    /// `state`): its `auth_events`, resolved through `lookup`, must be the right selection and
    /// allow it, and so must `state` when given. An `auth_events` entry `lookup` cannot find
    /// rejects the event when `missing_rejects`, and otherwise lets it through unchecked.
    fn authorize_history_event(
        &self,
        event: &Event,
        lookup: &Lookup<'_, B>,
        state: Option<&MapStateFetch<'_, B>>,
        missing_rejects: bool,
        room_has_create: bool,
    ) -> Verdict {
        let auth_ids = pipeline::decode_event_ids(event.json().get("auth_events"));
        let mut auth_events: Vec<&Event> = Vec::with_capacity(auth_ids.len());
        for id in &auth_ids {
            match lookup.event(id) {
                Some(found) => auth_events.push(found),
                None if missing_rejects => {
                    return Verdict::Rejected(format!(
                        "its auth event {id} is neither held, fetched nor in the batch"
                    ));
                }
                None => return Verdict::Unchecked,
            }
        }
        let mut auth_flat = FlatState::new();
        let mut auth_refs: Vec<AuthEventRef<'_>> = Vec::with_capacity(auth_events.len());
        for found in &auth_events {
            auth_flat.insert(
                found.header().event_type.clone(),
                found.header().state_key.clone().unwrap_or_default(),
                found.header().sender.clone(),
                content_of(found).clone(),
            );
            auth_refs.push(AuthEventRef {
                event_type: &found.header().event_type,
                state_key: found.header().state_key.as_deref().unwrap_or(""),
                rejected: found.header().flags.is_rejected(),
            });
        }
        // From room version 12 (MSC4291) the create event is never among `auth_events`: its ID
        // is the room ID, and every event is authorised as if it cited it.
        if self.rules.room_create_event_id_as_room_id
            && let Some(create) = state
                .and_then(|s| s.map.get(&("m.room.create".to_owned(), String::new())))
                .and_then(|id| lookup.event(id))
                .or_else(|| self.state_event("m.room.create", "").ok().flatten())
        {
            auth_flat.insert(
                create.header().event_type.clone(),
                String::new(),
                create.header().sender.clone(),
                content_of(create).clone(),
            );
        }
        let prev_ids = pipeline::decode_event_ids(event.json().get("prev_events"));
        let only_prev_is_create = prev_ids.len() == 1
            && lookup
                .event(&prev_ids[0])
                .is_some_and(|e| e.header().event_type == "m.room.create");
        let redacts = extract_redacts(event);
        let incoming = IncomingEvent {
            event_type: &event.header().event_type,
            sender: AsRef::<UserId>::as_ref(&event.header().sender),
            room_id: Some(&self.room_id),
            state_key: event.header().state_key.as_deref(),
            content: content_of(event),
            prev_event_count: prev_ids.len(),
            only_prev_event_is_room_create: only_prev_is_create,
            event_id: Some(event.event_id()),
            redacts: redacts.as_deref(),
        };
        if let Err(e) = auth::check_auth_events_selection(
            &self.rules,
            &incoming,
            &auth_refs,
            || -> Result<bool, AuthError> { Ok(room_has_create) },
        ) {
            return Verdict::Rejected(format!("its auth_events are not the right ones: {e}"));
        }
        if let Err(e) = auth::check_event_auth(&self.rules, &incoming, &auth_flat) {
            return Verdict::Rejected(format!("the state its auth_events imply rejects it: {e}"));
        }
        if let Some(state) = state
            && let Err(e) = auth::check_event_auth(&self.rules, &incoming, state)
        {
            return Verdict::Rejected(format!("the state before it rejects it: {e}"));
        }
        Verdict::Allowed
    }

    /// Stores what a state fetch brought ([`FetchedState::events`]) as outliers, each authorized
    /// first against its own `auth_events` (held, or brought by the same fetch), in topological
    /// order. Dropped, with a warning: an event for another room or room version, a non-state
    /// event, and one whose `auth_events` are out of reach or reject it. Returns how many were
    /// stored.
    ///
    /// # Errors
    /// [`RoomError::Store`], [`RoomError::Fenced`] or [`RoomError::State`] from persistence.
    fn store_fetched_state_events(&mut self, events: Vec<Event>) -> Result<usize, RoomError> {
        let mut pending: Vec<Event> = Vec::with_capacity(events.len());
        let mut seen: HashSet<OwnedEventId> = HashSet::new();
        for event in events {
            if self.event_id_index.contains_key(event.event_id())
                || !seen.insert(event.event_id().to_owned())
            {
                continue;
            }
            let room_id = event
                .json()
                .get("room_id")
                .and_then(CanonicalJsonValue::as_str);
            let wrong = if room_id != Some(self.room_id.as_str()) {
                Some("it is for another room")
            } else if event.header().room_version != self.room_version {
                Some("it is of another room version")
            } else if event.header().state_key.is_none() {
                Some("it is not a state event")
            } else {
                None
            };
            if let Some(why) = wrong {
                tracing::warn!(room_id = %self.room_id, event_id = %event.event_id(), why, "dropping an event a state fetch brought");
                continue;
            }
            pending.push(event);
        }
        pending.sort_by(topological_order);

        let mut accepted: HashMap<OwnedEventId, Event> = HashMap::with_capacity(pending.len());
        let mut order: Vec<OwnedEventId> = Vec::with_capacity(pending.len());
        for event in pending {
            let verdict = {
                let lookup = Lookup {
                    actor: self,
                    local: &accepted,
                };
                self.authorize_history_event(&event, &lookup, None, true, true)
            };
            if let Verdict::Rejected(reason) = verdict {
                tracing::warn!(
                    room_id = %self.room_id,
                    event_id = %event.event_id(),
                    reason,
                    "a state event a state fetch brought is not authorized by its auth_events; it is not stored"
                );
                continue;
            }
            order.push(event.event_id().to_owned());
            accepted.insert(event.event_id().to_owned(), event);
        }
        let outliers: Vec<Event> = order.iter().filter_map(|id| accepted.remove(id)).collect();
        self.persist_outliers(outliers)
    }

    /// `ids` as a [`StateMap`], from the events held for them. An ID not held (a fetch that
    /// failed, an event that failed authorization) or not a state event is left out, with a
    /// warning naming how many.
    fn state_map_from_ids(&self, ids: &[OwnedEventId]) -> StateMap {
        let mut state = StateMap::new();
        let mut absent = 0usize;
        for id in ids {
            match self
                .event_id_index
                .get(id)
                .and_then(|sn| self.events.get(sn))
                .and_then(|event| state_key_of(event).map(|key| (key, event)))
            {
                Some((key, event)) => {
                    state.insert(key, event.event_id().to_owned());
                }
                None => absent += 1,
            }
        }
        if absent > 0 {
            tracing::warn!(
                room_id = %self.room_id,
                absent,
                "state entries a server named are not held; the state at backfilled events lacks them"
            );
        }
        state
    }
}
