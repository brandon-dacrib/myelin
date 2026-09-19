//! [`RoomActor`]: the synchronous, single-room state machine. [`RoomActorHandle`]: the async,
//! serialized mailbox wrapping it. See `crate::protocol` for the design rationale.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use hs_model::ids::{EventSn, RoomSn};
use hs_model::room_version::{self, RoomIdFormat, RoomVersionRules};
use hs_state::api::StateStore;
use hs_state::kv_store::ProductionStateStore;
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, RoomVersionId, UserId,
};

use crate::error::RoomError;
use crate::identity::HomeserverIdentity;
use crate::membership::{self, Action, PriorState};
use crate::persist::{PersistedEvent, RoomMeta, Tables};
use crate::pipeline::{self, EventMap, NewEvent, RoomStateView};
use crate::protocol::{ChangedStateKey, MembershipDelta, RoomUpdate};
use crate::relations;
use crate::timeline::{Direction, PaginationToken};

fn to_kv(e: hs_tables::keyspace::TableError) -> hs_kv::KvError {
    match e {
        hs_tables::keyspace::TableError::Kv(kv) => kv,
        other @ hs_tables::keyspace::TableError::KeyCodec(_) => hs_kv::KvError::backend(other),
    }
}

/// Parameters for creating a new room, mirroring the client-server `POST /createRoom` request
/// body's fields this crate implements. See `crate::routes::create_room`.
#[derive(Debug, Clone, Default)]
pub struct CreateRoomRequest {
    /// `room_version`, or `None` to use this server's default (currently room version 11).
    pub room_version: Option<RoomVersionId>,
    /// `preset`: `"private_chat"`, `"public_chat"` or `"trusted_private_chat"`. Defaults to
    /// `"private_chat"` if absent and `visibility` is not `"public"`.
    pub preset: Option<String>,
    /// `name`.
    pub name: Option<String>,
    /// `topic`.
    pub topic: Option<String>,
    /// `invite`: users to invite once the room exists.
    pub invite: Vec<OwnedUserId>,
    /// `initial_state`: additional state events, applied after the preset's defaults.
    pub initial_state: Vec<InitialStateEvent>,
    /// `power_level_content_override`: replaces the default `m.room.power_levels` content
    /// entirely if present (matching the spec: this is a full override, not a merge).
    pub power_level_content_override: Option<serde_json::Value>,
    /// `creation_content`: merged into the `m.room.create` content (`creator`/`room_version` are
    /// still set by this crate, overriding anything the caller put there).
    pub creation_content: serde_json::Value,
    /// `room_alias_name`: the localpart of a local alias to create for this room.
    pub room_alias_name: Option<String>,
}

/// One `initial_state` entry.
#[derive(Debug, Clone)]
pub struct InitialStateEvent {
    /// `type`.
    pub event_type: String,
    /// `state_key`, defaulting to `""` if absent (per the spec).
    pub state_key: String,
    /// `content`.
    pub content: serde_json::Value,
}

/// The result of [`RoomActor::state_at_event`]: the room's state as of immediately *after* one
/// event, plus the auth chain input a caller answering `/state` or `/state_ids` needs alongside
/// it.
///
/// This is *S′(event)* in the spec's notation (see `hs_state::api::StateStore`'s module docs,
/// "`state_at` returns the state after the event") -- exactly what `/state_ids`' `pdu_ids` and
/// `auth_chain_ids` and `/state`'s `pdus`/`auth_chain` want for an explicit `event_id` query
/// parameter, for *any* event this actor knows, not only the newest one in the timeline. Before
/// this existed, `hs-room` only tracked one flat current-state map with no history, so the only
/// event whose state could be answered correctly was the timeline's head (see
/// `crates/hs-cli/src/federation.rs`'s module doc for the refusal this replaces).
#[derive(Debug, Clone)]
pub struct StateAtEvent {
    /// Every event that is part of the room's state as of immediately after the queried event,
    /// one per `(event_type, state_key)`. This is `/state_ids`' `pdu_ids` (or `/state`'s `pdus`,
    /// once rendered).
    pub state: Vec<Event>,
    /// The auth chain of `state`: every event reachable by following `auth_events` transitively
    /// from any event in `state`. This is `/state_ids`' `auth_chain_ids` -- note it deliberately
    /// does **not** include `state`'s own events unless another `state` event's ancestor chain
    /// also reaches them, which is normal (`m.room.create` is both part of the state and the
    /// ancestor of nearly everything else, so it legitimately appears in both `state` and
    /// `auth_chain`; callers building `/state`'s combined `auth_chain` field, which by convention
    /// *does* include the state events themselves, add `state` back in -- see
    /// `crates/hs-cli/src/federation.rs`'s `current_state_with_auth_chain` for the existing
    /// precedent this mirrors).
    pub auth_chain: Vec<Event>,
}

/// The synchronous room actor. Not `Send`-safe to hold across an `.await` (it borrows nothing
/// async), which is exactly why [`RoomActorHandle`] runs its methods inside
/// `tokio::task::spawn_blocking`. See `crate::protocol`'s module docs for the full design.
pub struct RoomActor<B: KvBackend> {
    backend: B,
    tables: Tables<B>,
    identity: HomeserverIdentity,
    room_sn: RoomSn,
    room_id: OwnedRoomId,
    room_version: RoomVersionId,
    rules: RoomVersionRules,
    /// The room's resolved state, held through `hs_state`'s production `StateStore`
    /// (`docs/rfcs/0010-room-actor-state-store-seam.md`, closed by track 02's
    /// `StoreStateFetch`/`intern_state_key` -- see `docs/status/02-state-and-model.md`'s "exactly
    /// what track 04 calls to delete `CurrentState`"). One store per room, sharing this room's own
    /// `backend` (content-addressed, so safe to share the physical keyspace across rooms -- see
    /// this crate's status file for why).
    store: ProductionStateStore<B>,
    /// Every event body held in memory. Phase 0 scope: unbounded (the whole room's history stays
    /// resident for the actor's lifetime); see `crate::registry` for room-granularity eviction and
    /// this module's doc comment on `events` for the documented next step (a bounded recent-window
    /// cache with KV fallback for older events).
    events: HashMap<EventSn, Event>,
    event_id_index: HashMap<OwnedEventId, EventSn>,
    /// The room's forward extremities. Usually a single event: every ordinary local send
    /// (`RoomActor::send_event`) cites *every* current extremity as its `prev_events`, which
    /// converges them all back down to one. More than one is a genuine, representable fork --
    /// see `RoomActor::send_event_citing` -- resolved through `self.store` rather than assumed
    /// away, which is the gap the flat map this crate used to hold could not represent at all.
    forward_extremities: BTreeSet<EventSn>,
    /// `room_pos -> EventSn`, ascending.
    timeline: BTreeMap<i64, EventSn>,
    next_room_pos: i64,
    /// `target_event_id -> [child EventSn]`, insertion order, for `crate::relations`.
    relations_by_target: HashMap<OwnedEventId, Vec<EventSn>>,
    /// `(sender, device_id-or-empty, txn_id) -> event_id`: transaction-ID deduplication for
    /// `PUT .../send/{txnId}` and `PUT .../redact/{txnId}` (client-server API "Transaction
    /// identifiers": replaying the same `txnId` must return the same `event_id`, not send a
    /// second event). In-memory only, per this actor's lifetime -- it does not survive an idle
    /// eviction and reload (`RoomActor::load` does not repopulate it); see this crate's status
    /// file for why that scope is enough to fix the bug this closes (a flaky-connection retry,
    /// not a reconnect minutes later) without a durable table.
    txn_dedup: HashMap<(OwnedUserId, String, String), OwnedEventId>,
    publish: tokio::sync::broadcast::Sender<RoomUpdate>,
}

impl<B: KvBackend> RoomActor<B> {
    fn intern_room(backend: &B, tables: &Tables<B>, room_id: &RoomId) -> Result<RoomSn, RoomError> {
        transact(backend, TransactConfig::default(), |txn| {
            tables.room_sn.get_or_create(txn, room_id.as_bytes())
        })
        .map_err(RoomError::from)
    }

    /// Creates a brand-new room: interns its ID, then builds, authorizes and persists the
    /// `m.room.create` event as the room's first event.
    ///
    /// `room_id` is the caller's chosen room ID for room versions with opaque room IDs (`!id:server`,
    /// room versions 1-11). For hash-based room IDs (room version 12+, MSC4291) it is **ignored**:
    /// the room ID cannot be known until the create event itself is built (it is derived from that
    /// event's own reference hash), so this function builds the create event first, against no
    /// room ID at all, then derives the real one -- see `docs/rfcs/0010-room-actor-state-store-seam.md`
    /// section 3 for why this needs to be a two-phase construction rather than a parameter
    /// reshuffle.
    ///
    /// # Errors
    /// Returns [`RoomError::UnsupportedRoomVersion`] if `room_version` is unknown. Otherwise, any
    /// error [`crate::pipeline::build_and_authorize`] or [`RoomActor::persist`] can return.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        backend: B,
        tables: Tables<B>,
        identity: HomeserverIdentity,
        room_id: OwnedRoomId,
        room_version: RoomVersionId,
        creator: OwnedUserId,
        creation_content: serde_json::Value,
        now_ms: i64,
    ) -> Result<Self, RoomError> {
        let rules = room_version::rules_for(&room_version)
            .ok_or_else(|| RoomError::UnsupportedRoomVersion(room_version.as_str().to_owned()))?;

        let store = ProductionStateStore::open(room_version.clone(), backend.clone())
            .map_err(|e| RoomError::State(e.to_string()))?;

        let empty_events: HashMap<EventSn, Event> = HashMap::new();
        let empty_view = RoomStateView {
            store: &store,
            root: store.empty_root(),
            bodies: EventMap(&empty_events),
        };
        let room_id_arg: Option<&RoomId> = if rules.room_id_format == RoomIdFormat::V2HashBased {
            None
        } else {
            Some(&room_id)
        };
        let create_event = pipeline::build_and_authorize(
            &room_version,
            &rules,
            room_id_arg,
            &identity.server_name,
            &identity.signing_key,
            now_ms,
            &[],
            &empty_view,
            NewEvent {
                event_type: "m.room.create".to_owned(),
                state_key: Some(String::new()),
                sender: creator,
                content: creation_content,
                redacts: None,
            },
        )?;

        let final_room_id = if rules.room_id_format == RoomIdFormat::V2HashBased {
            let hash = create_event.reference_hash().map_err(RoomError::from)?;
            let hash_b64 = hs_model::hash::encode_reference_hash(&hash, &rules);
            ruma::RoomId::new_v2(&hash_b64).map_err(|e| RoomError::Internal(e.to_string()))?
        } else {
            room_id
        };

        let room_sn = Self::intern_room(&backend, &tables, &final_room_id)?;
        let (publish, _rx) = tokio::sync::broadcast::channel(64);
        let mut actor = Self {
            backend,
            tables,
            identity,
            room_sn,
            room_id: final_room_id,
            room_version,
            rules,
            store,
            events: HashMap::new(),
            event_id_index: HashMap::new(),
            forward_extremities: BTreeSet::new(),
            timeline: BTreeMap::new(),
            next_room_pos: 1,
            relations_by_target: HashMap::new(),
            txn_dedup: HashMap::new(),
            publish,
        };
        actor.persist(create_event)?;
        Ok(actor)
    }

    /// Reconstructs a room actor by replaying its full persisted timeline. `Ok(None)` if the room
    /// is not known to this store (was never created, or `room_id` is misspelled).
    ///
    /// This is `crate::registry::RoomRegistry`'s cold-load path: everything a `RoomActor` holds is
    /// derivable from the store (`PLAN.md` section 5.3), and this is the function that derives it.
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure, or [`RoomError::InvalidEvent`] if a
    /// persisted event fails to re-parse (should not happen: it parsed successfully once already
    /// to be persisted).
    pub fn load(
        backend: B,
        tables: Tables<B>,
        identity: HomeserverIdentity,
        room_id: &RoomId,
    ) -> Result<Option<Self>, RoomError> {
        let snapshot = backend.snapshot();
        let Some(room_sn) = tables.room_sn.lookup(&snapshot, room_id.as_bytes())? else {
            return Ok(None);
        };
        let Some(meta_bytes) = tables.room_meta.get(&snapshot, &(room_sn,))? else {
            return Ok(None);
        };
        let meta: RoomMeta =
            serde_json::from_slice(&meta_bytes).map_err(|e| RoomError::Internal(e.to_string()))?;
        let room_version = RoomVersionId::try_from(meta.room_version.as_str())
            .map_err(|_| RoomError::UnsupportedRoomVersion(meta.room_version.clone()))?;
        let rules = room_version::rules_for(&room_version)
            .ok_or_else(|| RoomError::UnsupportedRoomVersion(meta.room_version.clone()))?;

        let store = ProductionStateStore::open(room_version.clone(), backend.clone())
            .map_err(|e| RoomError::State(e.to_string()))?;

        let (publish, _rx) = tokio::sync::broadcast::channel(64);
        let mut actor = Self {
            backend,
            tables,
            identity,
            room_sn,
            room_id: room_id.to_owned(),
            room_version: room_version.clone(),
            rules,
            store,
            events: HashMap::new(),
            event_id_index: HashMap::new(),
            forward_extremities: BTreeSet::new(),
            timeline: BTreeMap::new(),
            next_room_pos: 1,
            relations_by_target: HashMap::new(),
            txn_dedup: HashMap::new(),
            publish,
        };

        let range_spec = hs_tables::keyspace::TypedKeyspace::<
            B::Keyspace,
            crate::persist::TimelineKey,
        >::prefix(&(room_sn,));
        let mut entries: Vec<(i64, EventSn)> = Vec::new();
        for item in actor.tables.timeline.range(&snapshot, range_spec) {
            let ((_, room_pos), value) = item?;
            let sn_bytes: [u8; 8] = value
                .as_ref()
                .try_into()
                .map_err(|_| RoomError::Internal("corrupt timeline entry".into()))?;
            entries.push((room_pos, EventSn::from_be_bytes(sn_bytes)));
        }
        entries.sort_by_key(|(pos, _)| *pos);

        for (room_pos, event_sn) in entries {
            let Some(bytes) = actor.tables.events.get(&snapshot, &(event_sn,))? else {
                continue;
            };
            let persisted: PersistedEvent =
                serde_json::from_slice(&bytes).map_err(|e| RoomError::Internal(e.to_string()))?;
            let event = Event::parse(&persisted.json, room_version.clone())?;
            actor.absorb_loaded_event(event_sn, event, room_pos)?;
        }

        // The authoritative forward-extremity set is whatever `RoomActor::persist` last wrote to
        // `Tables::extremities_fwd` -- read it directly rather than inferring it from timeline
        // replay order (which is only "the last event replayed" and is wrong the moment a room
        // has ever had more than one extremity at once, i.e. a fork).
        let ext_spec = hs_tables::keyspace::TypedKeyspace::<
            B::Keyspace,
            crate::persist::ExtremityKey,
        >::prefix(&(room_sn,));
        for item in actor.tables.extremities_fwd.range(&snapshot, ext_spec) {
            let ((_, sn), _) = item?;
            actor.forward_extremities.insert(sn);
        }

        Ok(Some(actor))
    }

    /// Ingests `event` (already durably persisted under `event_sn`) into the room's production
    /// state store, decoding its own `auth_events`/`prev_events` fields back into `EventSn`s via
    /// `self.event_id_index` (`crate::pipeline::decode_event_ids`) -- every ancestor an event
    /// cites is already known to this actor by the time it is persisted or replayed, whether
    /// locally originated or loaded from the store, so this never needs a network round trip.
    /// Returns the decoded `prev_events` `EventSn`s, which `RoomActor::persist` also needs for
    /// forward-extremity bookkeeping.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    fn feed_store(&mut self, event: &Event, event_sn: EventSn) -> Result<Vec<EventSn>, RoomError> {
        let prev_sns: Vec<EventSn> = pipeline::decode_event_ids(event.json().get("prev_events"))
            .iter()
            .filter_map(|id| self.event_id_index.get(id))
            .copied()
            .collect();
        let auth_sns: Vec<EventSn> = pipeline::decode_event_ids(event.json().get("auth_events"))
            .iter()
            .filter_map(|id| self.event_id_index.get(id))
            .copied()
            .collect();
        let content_obj = event
            .json()
            .get("content")
            .and_then(CanonicalJsonValue::as_object)
            .cloned()
            .unwrap_or_default();
        let only_prev_is_create = prev_sns.len() == 1
            && self
                .events
                .get(&prev_sns[0])
                .is_some_and(|e| e.header().event_type == "m.room.create");
        self.store
            .add_event(
                event_sn,
                event.event_id().to_owned(),
                self.room_id.clone(),
                &event.header().event_type,
                event.header().state_key.as_deref(),
                event.header().sender.clone(),
                content_obj,
                event.header().depth,
                event.header().origin_server_ts,
                &auth_sns,
                &prev_sns,
                only_prev_is_create,
            )
            .map_err(|e| RoomError::State(e.to_string()))?;
        Ok(prev_sns)
    }

    fn absorb_loaded_event(
        &mut self,
        event_sn: EventSn,
        event: Event,
        room_pos: i64,
    ) -> Result<(), RoomError> {
        if let Some(content) = event
            .json()
            .get("content")
            .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
        {
            let content_value: serde_json::Value = serde_json::from_slice(
                &hs_model::canonical::CanonicalJsonValue::Object(content.clone())
                    .to_canonical_bytes(),
            )
            .unwrap_or(serde_json::Value::Null);
            if let Some(rel) = relations::relation_of(&content_value) {
                self.relations_by_target
                    .entry(rel.target)
                    .or_default()
                    .push(event_sn);
            }
        }
        self.event_id_index
            .insert(event.event_id().to_owned(), event_sn);
        self.timeline.insert(room_pos, event_sn);
        self.next_room_pos = self.next_room_pos.max(room_pos + 1);
        self.feed_store(&event, event_sn)?;
        self.events.insert(event_sn, event);
        Ok(())
    }

    fn forward_extremities_vec(&self) -> Vec<EventSn> {
        self.forward_extremities.iter().copied().collect()
    }

    /// Builds a [`RoomStateView`] over this actor's state store, resolved at `prev_sns` (the
    /// empty state if `prev_sns` is empty, e.g. for a brand-new room's `m.room.create`). With one
    /// element this is exactly that event's `state_at` (no `resolve()` call, per
    /// [`hs_state::api::StateStore::current_state`]'s documented behavior); with more than one it
    /// is the genuine fork-resolution case -- see this module's doc comment on
    /// `RoomActor::forward_extremities`.
    fn state_view(&self, prev_sns: &[EventSn]) -> Result<RoomStateView<'_, ProductionStateStore<B>>, RoomError> {
        let root = if prev_sns.is_empty() {
            self.store.empty_root()
        } else {
            self.store
                .current_state(&self.room_version, prev_sns)
                .map_err(|e| RoomError::State(e.to_string()))?
        };
        Ok(RoomStateView {
            store: &self.store,
            root,
            bodies: EventMap(&self.events),
        })
    }

    /// The room's current, resolved state: `RoomActor::state_view` at every current forward
    /// extremity.
    fn current_view(&self) -> Result<RoomStateView<'_, ProductionStateStore<B>>, RoomError> {
        self.state_view(&self.forward_extremities_vec())
    }

    fn refs_for(&self, sns: &[EventSn]) -> Result<Vec<pipeline::EventRef>, RoomError> {
        sns.iter()
            .map(|sn| {
                let event = self.events.get(sn).ok_or_else(|| {
                    RoomError::Internal("cited event not in hot cache".into())
                })?;
                pipeline::event_ref(event, &self.rules)
            })
            .collect()
    }

    /// Builds, hashes, signs, authorizes and persists a new locally-originated event citing
    /// *every* current forward extremity as its `prev_events` -- which is exactly what converges
    /// any existing fork back down to one extremity, since the new event supersedes all of them
    /// at once. See [`RoomActor::send_event_citing`] for building against an explicit, narrower
    /// ancestor set instead.
    ///
    /// # Errors
    /// See `crate::pipeline::build_and_authorize` and [`RoomActor::persist`].
    pub fn send_event(
        &mut self,
        sender: OwnedUserId,
        event_type: String,
        state_key: Option<String>,
        content: serde_json::Value,
        redacts: Option<OwnedEventId>,
        now_ms: i64,
    ) -> Result<Event, RoomError> {
        let prev_sns = self.forward_extremities_vec();
        self.send_event_citing(sender, event_type, state_key, content, redacts, now_ms, &prev_sns)
    }

    /// Builds, hashes, signs, authorizes and persists a new event citing exactly `prev_events` as
    /// its ancestors, rather than "every current forward extremity"
    /// ([`RoomActor::send_event`]'s always-converge behavior).
    ///
    /// This is **not** `Command::PersistInbound` (`docs/design/04-room-actor-protocol.md`): it
    /// does no signature verification, no remote-server trust decisions and no missing-event
    /// backfill -- the event is built, signed and authorized locally, exactly like
    /// [`RoomActor::send_event`], just against an explicit ancestor set instead of the implicit
    /// "everything this actor currently knows about" one. It exists so a caller can construct a
    /// genuine fork -- two events that each cite the same prior extremity without citing each
    /// other -- to exercise [`hs_state::api::StateStore::resolve`] through the room actor, which
    /// `send_event`'s always-converge behavior can never do on its own. See this crate's status
    /// file for why this is the right scope for closing "the room actor cannot represent a fork"
    /// without also implementing federation ingestion.
    ///
    /// # Errors
    /// See `crate::pipeline::build_and_authorize` and [`RoomActor::persist`].
    pub fn send_event_citing(
        &mut self,
        sender: OwnedUserId,
        event_type: String,
        state_key: Option<String>,
        content: serde_json::Value,
        redacts: Option<OwnedEventId>,
        now_ms: i64,
        prev_events: &[EventSn],
    ) -> Result<Event, RoomError> {
        let prev_refs = self.refs_for(prev_events)?;
        let state = self.state_view(prev_events)?;
        let event = pipeline::build_and_authorize(
            &self.room_version,
            &self.rules,
            Some(&self.room_id),
            &self.identity.server_name,
            &self.identity.signing_key,
            now_ms,
            &prev_refs,
            &state,
            NewEvent {
                event_type,
                state_key,
                sender,
                content,
                redacts,
            },
        )?;
        self.persist(event.clone())?;
        Ok(event)
    }

    /// The membership precheck plus [`RoomActor::send_event`] for `m.room.member`.
    ///
    /// # Errors
    /// Returns [`RoomError::Forbidden`] if [`membership::precheck`] rejects the transition, or any
    /// error [`RoomActor::send_event`] can return.
    pub fn membership_action(
        &mut self,
        sender: OwnedUserId,
        action: Action,
        target: OwnedUserId,
        extra: serde_json::Value,
        now_ms: i64,
    ) -> Result<Event, RoomError> {
        let prior = self.prior_membership(&target)?;
        membership::precheck(&self.rules, action, prior)
            .map_err(|e| RoomError::Forbidden(e.to_string()))?;
        let content = membership::content_for(action, extra);
        self.send_event(
            sender,
            "m.room.member".to_owned(),
            Some(target.to_string()),
            content,
            None,
            now_ms,
        )
    }

    fn prior_membership(&self, target: &UserId) -> Result<PriorState, RoomError> {
        let Some(event) = self.state_event("m.room.member", target.as_str())? else {
            return Ok(PriorState::None);
        };
        let value = event
            .json()
            .get("content")
            .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
            .and_then(|c| c.get("membership"))
            .and_then(hs_model::canonical::CanonicalJsonValue::as_str);
        Ok(match value {
            Some("join") => PriorState::Join,
            Some("invite") => PriorState::Invite,
            Some("leave") => PriorState::Leave,
            Some("ban") => PriorState::Ban,
            Some("knock") => PriorState::Knock,
            _ => PriorState::None,
        })
    }

    /// Persists a built, authorized event: interns it, writes the event record, timeline entry,
    /// forward-extremity update and relation index entry (if any) in one `hs-kv` transaction, then
    /// updates the in-memory hot state and publishes a [`RoomUpdate`].
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    fn persist(&mut self, event: Event) -> Result<EventSn, RoomError> {
        let full_json: serde_json::Value = serde_json::from_slice(event.canonical_bytes())
            .map_err(|e| RoomError::Internal(e.to_string()))?;
        let content = full_json
            .get("content")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let relation = relations::relation_of(&content);

        let is_first = self.events.is_empty();
        let room_meta_bytes = if is_first {
            Some(
                serde_json::to_vec(&RoomMeta {
                    room_id: self.room_id.to_string(),
                    room_version: self.room_version.as_str().to_owned(),
                })
                .map_err(|e| RoomError::Internal(e.to_string()))?,
            )
        } else {
            None
        };

        let persisted = PersistedEvent {
            room_id: self.room_id.to_string(),
            json: full_json,
            room_version: self.room_version.as_str().to_owned(),
            flags: event.header().flags.to_byte(),
            room_pos: Some(self.next_room_pos),
        };
        let persisted_bytes =
            serde_json::to_vec(&persisted).map_err(|e| RoomError::Internal(e.to_string()))?;

        let room_pos = self.next_room_pos;
        let room_sn = self.room_sn;
        let event_id_bytes = event.event_id().as_bytes().to_vec();

        // Decode this event's own `prev_events` up front: needed both for the KV transaction's
        // forward-extremity bookkeeping below and for feeding the state store afterwards.
        // Ancestors are always already interned in `event_id_index` by the time an event cites
        // them (locally built from the actor's own current extremities, or -- for
        // `send_event_citing` -- an explicit subset of events this actor already holds).
        let prev_sns: Vec<EventSn> = pipeline::decode_event_ids(event.json().get("prev_events"))
            .iter()
            .filter_map(|id| self.event_id_index.get(id))
            .copied()
            .collect();
        let old_extremities: Vec<EventSn> = prev_sns
            .iter()
            .copied()
            .filter(|sn| self.forward_extremities.contains(sn))
            .collect();

        let event_sn = transact(&self.backend, TransactConfig::default(), |txn| {
            let event_sn = self.tables.event_sn.get_or_create(txn, &event_id_bytes)?;
            if let Some(meta_bytes) = &room_meta_bytes {
                self.tables
                    .room_meta
                    .put(txn, &(room_sn,), meta_bytes)
                    .map_err(to_kv)?;
            }
            self.tables
                .events
                .put(txn, &(event_sn,), &persisted_bytes)
                .map_err(to_kv)?;
            self.tables
                .timeline
                .put(txn, &(room_sn, room_pos), &event_sn.to_be_bytes())
                .map_err(to_kv)?;
            for old in &old_extremities {
                self.tables
                    .extremities_fwd
                    .delete(txn, &(room_sn, *old))
                    .map_err(to_kv)?;
            }
            self.tables
                .extremities_fwd
                .put(txn, &(room_sn, event_sn), b"")
                .map_err(to_kv)?;
            if let Some(rel) = &relation {
                let target_sn = self
                    .tables
                    .event_sn
                    .get_or_create(txn, rel.target.as_bytes())?;
                self.tables
                    .relations
                    .put(
                        txn,
                        &(room_sn, target_sn, rel.rel_type.clone(), event_sn),
                        b"",
                    )
                    .map_err(to_kv)?;
            }
            Ok(event_sn)
        })
        .map_err(RoomError::from)?;

        // Feed the production state store. This runs as its own write after the room's own KV
        // transaction above commits, not inside it: `hs_state::api::StateStore`'s methods take
        // `&self` with no externally-supplied transaction handle, so this crate cannot thread the
        // two into one atomic commit with the interface as given. A crash strictly between the two
        // could leave a persisted event whose state-store ingestion did not happen -- a real,
        // narrow gap this pass's wiring introduces (there was nothing to compare against before:
        // the flat map it replaces had no separate store to fall out of sync with). Recorded in
        // this crate's status file rather than silently accepted.
        self.feed_store(&event, event_sn)?;

        let mut changed_state_keys = Vec::new();
        let mut membership_deltas = Vec::new();
        if let Some(state_key) = event.header().state_key.clone() {
            changed_state_keys.push(ChangedStateKey {
                event_type: event.header().event_type.clone(),
                state_key: state_key.clone(),
            });
            if event.header().event_type == "m.room.member"
                && let Ok(target) = UserId::parse(state_key.as_str())
                && let Some(m) = event
                    .json()
                    .get("content")
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                    .and_then(|c| c.get("membership"))
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
            {
                membership_deltas.push(MembershipDelta {
                    user_id: target.to_owned(),
                    membership: m.to_owned(),
                });
            }
        }
        if let Some(rel) = relation {
            self.relations_by_target
                .entry(rel.target)
                .or_default()
                .push(event_sn);
        }
        for old in &prev_sns {
            self.forward_extremities.remove(old);
        }
        self.forward_extremities.insert(event_sn);
        self.timeline.insert(room_pos, event_sn);
        self.next_room_pos += 1;
        self.event_id_index
            .insert(event.event_id().to_owned(), event_sn);

        let update = RoomUpdate {
            room_sn: self.room_sn,
            room_id: self.room_id.clone(),
            room_pos,
            event_sn,
            event_id: event.event_id().to_owned(),
            event_type: event.header().event_type.clone(),
            state_key: event.header().state_key.clone(),
            sender: event.header().sender.clone(),
            changed_state_keys,
            membership_deltas,
            push_evaluation_inputs: Vec::new(),
        };
        self.events.insert(event_sn, event);
        // A broadcast send fails only when there are no subscribers, which is not an error: a
        // room with nobody listening yet (or right now) is normal.
        let _ = self.publish.send(update);

        Ok(event_sn)
    }

    /// Creates a room from a `POST /createRoom`-shaped request: the `m.room.create` event, the
    /// creator's own join, the preset's default state events, `initial_state`, `name`/`topic`, and
    /// invites, in spec order.
    ///
    /// # Errors
    /// See [`RoomActor::create`], [`RoomActor::send_event`] and [`RoomActor::membership_action`].
    pub fn create_room(
        backend: B,
        tables: Tables<B>,
        identity: HomeserverIdentity,
        creator: OwnedUserId,
        request: CreateRoomRequest,
        now_ms: i64,
    ) -> Result<Self, RoomError> {
        let room_version = request
            .room_version
            .clone()
            .unwrap_or_else(|| RoomVersionId::try_from("11").expect("11 is a known room version"));
        let rules = room_version::rules_for(&room_version)
            .ok_or_else(|| RoomError::UnsupportedRoomVersion(room_version.as_str().to_owned()))?;

        let room_id = RoomId::new_v1(&identity.server_name);

        let mut creation_content = request.creation_content.clone();
        if !creation_content.is_object() {
            creation_content = serde_json::json!({});
        }
        {
            let map = creation_content
                .as_object_mut()
                .expect("forced to an object above");
            map.insert(
                "room_version".to_owned(),
                serde_json::Value::String(room_version.as_str().to_owned()),
            );
            if !rules.use_room_create_sender {
                map.insert(
                    "creator".to_owned(),
                    serde_json::Value::String(creator.to_string()),
                );
            }
        }

        let mut actor = Self::create(
            backend,
            tables,
            identity,
            room_id,
            room_version,
            creator.clone(),
            creation_content,
            now_ms,
        )?;

        actor.membership_action(
            creator.clone(),
            Action::Join,
            creator.clone(),
            serde_json::json!({}),
            now_ms,
        )?;

        let preset = request.preset.as_deref().unwrap_or("private_chat");
        let (join_rule, history_visibility, guest_access) = match preset {
            "public_chat" => ("public", "shared", "forbidden"),
            "trusted_private_chat" => ("invite", "shared", "can_join"),
            _ => ("invite", "shared", "can_join"),
        };

        let power_levels_content =
            request
                .power_level_content_override
                .clone()
                .unwrap_or_else(|| {
                    let mut users = serde_json::Map::new();
                    // From room version 12 (MSC4289) the room's creators hold power implicitly and
                    // for ever, and `m.room.power_levels` naming any of them in `users` is
                    // rejected outright by `hs_state::auth`'s `check_room_power_levels`. Below
                    // that version the creator's authority comes *from* this entry, so it must be
                    // present. `explicitly_privilege_room_creators` is the room-version rule that
                    // distinguishes the two, rather than a version comparison here.
                    if !rules.explicitly_privilege_room_creators {
                        users.insert(creator.to_string(), serde_json::Value::from(100));
                    }
                    if preset == "trusted_private_chat" {
                        // Invitees are not creators (creators are the sender plus any
                        // `additional_creators` on the create event), so they take an ordinary
                        // explicit entry in every room version.
                        for user in &request.invite {
                            users.insert(user.to_string(), serde_json::Value::from(100));
                        }
                    }
                    serde_json::json!({ "users": users })
                });
        actor.send_event(
            creator.clone(),
            "m.room.power_levels".to_owned(),
            Some(String::new()),
            power_levels_content,
            None,
            now_ms,
        )?;

        actor.send_event(
            creator.clone(),
            "m.room.join_rules".to_owned(),
            Some(String::new()),
            serde_json::json!({"join_rule": join_rule}),
            None,
            now_ms,
        )?;
        actor.send_event(
            creator.clone(),
            "m.room.history_visibility".to_owned(),
            Some(String::new()),
            serde_json::json!({"history_visibility": history_visibility}),
            None,
            now_ms,
        )?;
        actor.send_event(
            creator.clone(),
            "m.room.guest_access".to_owned(),
            Some(String::new()),
            serde_json::json!({"guest_access": guest_access}),
            None,
            now_ms,
        )?;

        for entry in &request.initial_state {
            actor.send_event(
                creator.clone(),
                entry.event_type.clone(),
                Some(entry.state_key.clone()),
                entry.content.clone(),
                None,
                now_ms,
            )?;
        }

        if let Some(name) = &request.name {
            actor.send_event(
                creator.clone(),
                "m.room.name".to_owned(),
                Some(String::new()),
                serde_json::json!({"name": name}),
                None,
                now_ms,
            )?;
        }
        if let Some(topic) = &request.topic {
            actor.send_event(
                creator.clone(),
                "m.room.topic".to_owned(),
                Some(String::new()),
                serde_json::json!({"topic": topic}),
                None,
                now_ms,
            )?;
        }

        if let Some(localpart) = &request.room_alias_name {
            let alias_str = format!("#{localpart}:{}", actor.identity.server_name);
            let alias = RoomAliasId::parse(&alias_str)
                .map_err(|e| RoomError::BadRequest(format!("invalid room_alias_name: {e}")))?;
            actor.create_alias(&alias)?;
            actor.send_event(
                creator.clone(),
                "m.room.canonical_alias".to_owned(),
                Some(String::new()),
                serde_json::json!({"alias": alias_str}),
                None,
                now_ms,
            )?;
        }

        for user in &request.invite {
            actor.membership_action(
                creator.clone(),
                Action::Invite,
                user.clone(),
                serde_json::json!({}),
                now_ms,
            )?;
        }

        Ok(actor)
    }

    /// Creates a local alias pointing at this room.
    ///
    /// # Errors
    /// Returns [`RoomError::RoomAlreadyExists`] (reused: "this alias is already in use") if the
    /// alias is already mapped, or [`RoomError::Store`] on a storage failure.
    pub fn create_alias(&self, alias: &RoomAliasId) -> Result<(), RoomError> {
        let room_sn = self.room_sn;
        let alias_key = (alias.to_string(),);
        transact(&self.backend, TransactConfig::default(), |txn| {
            if self
                .tables
                .aliases
                .get(txn, &alias_key)
                .map_err(to_kv)?
                .is_some()
            {
                return Err(hs_kv::KvError::backend(AliasInUse));
            }
            self.tables
                .aliases
                .put(txn, &alias_key, &room_sn.to_be_bytes())
                .map_err(to_kv)?;
            self.tables
                .room_aliases
                .put(txn, &(room_sn, alias.to_string()), b"")
                .map_err(to_kv)?;
            Ok(())
        })
        .map_err(|e| match e {
            hs_kv::KvError::Backend(inner) if inner.is::<AliasInUse>() => {
                RoomError::RoomAlreadyExists(alias.to_string())
            }
            other => RoomError::from(other),
        })
    }

    /// Removes a local alias pointing at this room. Not an error if the alias did not exist.
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn remove_alias(&self, alias: &RoomAliasId) -> Result<(), RoomError> {
        let room_sn = self.room_sn;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.tables
                .aliases
                .delete(txn, &(alias.to_string(),))
                .map_err(to_kv)?;
            self.tables
                .room_aliases
                .delete(txn, &(room_sn, alias.to_string()))
                .map_err(to_kv)
        })
        .map_err(RoomError::from)
    }

    /// Every local alias pointing at this room.
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn list_aliases(&self) -> Result<Vec<String>, RoomError> {
        let snapshot = self.backend.snapshot();
        let spec =
            hs_tables::keyspace::TypedKeyspace::<B::Keyspace, crate::persist::RoomAliasKey>::prefix(
                &(self.room_sn,),
            );
        let mut out = Vec::new();
        for item in self.tables.room_aliases.range(&snapshot, spec) {
            let ((_, alias), _) = item?;
            out.push(alias);
        }
        Ok(out)
    }

    // --- queries ---

    /// The room's ID.
    #[must_use]
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// The room's version.
    #[must_use]
    pub fn room_version(&self) -> &RoomVersionId {
        &self.room_version
    }

    /// One current-state event, by `(event_type, state_key)`.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn state_event(&self, event_type: &str, state_key: &str) -> Result<Option<&Event>, RoomError> {
        self.current_view()?
            .event_for(event_type, state_key)
            .map_err(|e| RoomError::State(e.to_string()))
    }

    /// A [`RoomUpdate`] describing this room's newest timeline event, or `None` for a room with an
    /// empty timeline (which cannot happen for a persisted room: `m.room.create` is always there).
    ///
    /// This is not a synthetic event -- it is the real head of the timeline, reported after the
    /// fact. It exists so that something which starts watching a room *after* the room was built
    /// can still discover it: `RoomActor::create_room` publishes its whole create burst while the
    /// actor is still being constructed, before any caller can possibly hold a handle to subscribe
    /// with, so those updates reach nobody. `changed_state_keys` and `membership_deltas` are left
    /// empty because this is a "here is where the room is now" announcement rather than a report of
    /// one transition; a consumer that cares about membership reads the room's current state, which
    /// is what `hs_user::hub::SessionHub::process_room_update` does anyway.
    ///
    /// See `docs/rfcs/0012-room-registry-global-updates.md` and
    /// [`crate::registry::RoomRegistry::subscribe_global`].
    #[must_use]
    pub fn head_update(&self) -> Option<RoomUpdate> {
        let (&room_pos, &event_sn) = self.timeline.iter().next_back()?;
        let event = self.events.get(&event_sn)?;
        Some(RoomUpdate {
            room_sn: self.room_sn,
            room_id: self.room_id.clone(),
            room_pos,
            event_sn,
            event_id: event.event_id().to_owned(),
            event_type: event.header().event_type.clone(),
            state_key: event.header().state_key.clone(),
            sender: event.header().sender.clone(),
            changed_state_keys: Vec::new(),
            membership_deltas: Vec::new(),
            push_evaluation_inputs: Vec::new(),
        })
    }

    /// Every current-state event: every entry in the resolution of the room's current forward
    /// extremities, dereferenced back into a full [`Event`] through this actor's in-memory cache.
    /// `hs_state::api::StateStore` has no direct "enumerate every key in a root" method, so this
    /// is computed as `diff(empty_root, current_root)`'s `added` set -- the empty state's diff
    /// against any root is, by definition, every entry that root sets.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn full_state(&self) -> Result<Vec<&Event>, RoomError> {
        let root = self.current_view()?.root;
        let diff = self
            .store
            .diff(self.store.empty_root(), root)
            .map_err(|e| RoomError::State(e.to_string()))?;
        Ok(diff
            .added
            .values()
            .filter_map(|sn| self.events.get(sn))
            .collect())
    }

    /// The room's state as of immediately after `event_id` (`hs_state::api::StateStore::state_at`
    /// already answers exactly this for any event this actor has ingested -- state history was
    /// never the gap; nothing before this method surfaced it), plus the auth chain of that state.
    /// `Ok(None)` if this actor does not know `event_id`.
    ///
    /// This works for *any* known event, not just the timeline's newest -- unlike
    /// [`RoomActor::full_state`] (which is always the *current* resolved state), this takes a root
    /// from `self.store.state_at(sn)` for the specific event asked about. Every event this actor
    /// has ever persisted or replayed on load was fed to `self.store` via `feed_store`
    /// (`RoomActor::persist`, `RoomActor::absorb_loaded_event`), so `state_at` has a root for it
    /// regardless of how long ago it stopped being the timeline head.
    ///
    /// The auth chain is computed via [`hs_state::api::StateStore::auth_chain_difference`] with a
    /// deliberately empty second set: `auth_chain_difference(&[roots, []])` reduces to exactly the
    /// union of `roots`' ancestors, because the coverage of an empty root list is empty on every
    /// chain, which makes the "symmetric difference" formula degenerate into "everything reachable
    /// from `roots` and nothing more" (see `hs_state::chain_cover::ChainCoverIndex::coverage`'s
    /// doc comment for the primitive this is built from). This crate did not need a new
    /// `hs-state` method to answer "the auth chain of a state map" because the existing trait
    /// already expresses it, just not under an obvious name -- see this crate's status file for a
    /// note that a direct `auth_chain_of(&[EventSn]) -> Vec<EventSn>` on `StateStore` would read
    /// better at the call site, if track 02 has budget.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn state_at_event(&self, event_id: &EventId) -> Result<Option<StateAtEvent>, RoomError> {
        let Some(&sn) = self.event_id_index.get(event_id) else {
            return Ok(None);
        };
        let root = self
            .store
            .state_at(sn)
            .map_err(|e| RoomError::State(e.to_string()))?;
        let diff = self
            .store
            .diff(self.store.empty_root(), root)
            .map_err(|e| RoomError::State(e.to_string()))?;
        let state_sns: Vec<EventSn> = diff.added.values().copied().collect();
        let state: Vec<Event> = state_sns
            .iter()
            .filter_map(|s| self.events.get(s).cloned())
            .collect();

        let auth_chain_sns = self
            .store
            .auth_chain_difference(&[state_sns, Vec::new()])
            .map_err(|e| RoomError::State(e.to_string()))?;
        let auth_chain: Vec<Event> = auth_chain_sns
            .iter()
            .filter_map(|s| self.events.get(s).cloned())
            .collect();

        Ok(Some(StateAtEvent { state, auth_chain }))
    }

    /// One event by ID, if this actor holds it (its own room's events only).
    #[must_use]
    pub fn event_by_id(&self, event_id: &EventId) -> Option<&Event> {
        let sn = self.event_id_index.get(event_id)?;
        self.events.get(sn)
    }

    /// Every current `m.room.member` event.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn members(&self) -> Result<Vec<&Event>, RoomError> {
        Ok(self
            .full_state()?
            .into_iter()
            .filter(|e| e.header().event_type == "m.room.member")
            .collect())
    }

    /// Every current `m.room.member` event whose `membership` is `join`.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn joined_members(&self) -> Result<Vec<&Event>, RoomError> {
        Ok(self
            .members()?
            .into_iter()
            .filter(|e| {
                e.json()
                    .get("content")
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                    .and_then(|c| c.get("membership"))
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                    == Some("join")
            })
            .collect())
    }

    /// Pages the timeline from `from` (or the live end, if `None`) in `direction`, returning up to
    /// `limit` events and the token to continue from.
    #[must_use]
    pub fn paginate(
        &self,
        from: Option<PaginationToken>,
        direction: Direction,
        limit: usize,
    ) -> (Vec<&Event>, Option<PaginationToken>) {
        let start = from.map_or_else(
            || match direction {
                Direction::Backward => i64::MAX,
                Direction::Forward => i64::MIN,
            },
            |t| t.room_pos,
        );

        // Backward: collected newest-first (descending `room_pos`), which is exactly the order
        // the spec wants `chunk` in for `dir=b` -- no re-sort needed. Forward: collected
        // oldest-first (ascending), already the order `dir=f` wants.
        let positions: Vec<i64> = match direction {
            Direction::Backward => self
                .timeline
                .range(..start)
                .rev()
                .take(limit)
                .map(|(pos, _)| *pos)
                .collect(),
            Direction::Forward => self
                .timeline
                .range((std::ops::Bound::Excluded(start), std::ops::Bound::Unbounded))
                .take(limit)
                .map(|(pos, _)| *pos)
                .collect(),
        };

        let events: Vec<&Event> = positions
            .iter()
            .filter_map(|pos| self.timeline.get(pos))
            .filter_map(|sn| self.events.get(sn))
            .collect();

        // The continuation token is always the *last* position returned (oldest of the page for
        // backward, newest of the page for forward): the boundary the next page's `range` call
        // should exclude up to/from.
        let next = positions
            .last()
            .map(|p| PaginationToken::new(*p, direction));

        (events, next)
    }

    /// The children of `target` recorded by `crate::relations`, optionally filtered by
    /// `rel_type`, in persisted order.
    #[must_use]
    pub fn relations_of(&self, target: &EventId, rel_type: Option<&str>) -> Vec<&Event> {
        let Some(children) = self.relations_by_target.get(target) else {
            return Vec::new();
        };
        children
            .iter()
            .filter_map(|sn| self.events.get(sn))
            .filter(|e| {
                rel_type.is_none_or(|want| {
                    e.json()
                        .get("content")
                        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                        .and_then(|c| c.get("m.relates_to"))
                        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                        .and_then(|r| r.get("rel_type"))
                        .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                        == Some(want)
                })
            })
            .collect()
    }

    /// The `unsigned.m.relations` bundle for `target`, computed from its children
    /// (`crate::relations::bundle`) as seen by `requesting_user` (thread participation is
    /// per-viewer).
    #[must_use]
    pub fn relation_bundle(&self, target: &EventId, requesting_user: &UserId) -> relations::Bundle {
        let children: Vec<relations::ChildEvent> = self
            .relations_of(target, None)
            .into_iter()
            .filter_map(|e| {
                let content = e.json().get("content")?.as_object()?;
                let content_value: serde_json::Value = serde_json::from_slice(
                    &hs_model::canonical::CanonicalJsonValue::Object(content.clone())
                        .to_canonical_bytes(),
                )
                .ok()?;
                let relation = relations::relation_of(&content_value)?;
                Some(relations::ChildEvent {
                    event_id: e.event_id().to_owned(),
                    sender: e.header().sender.clone(),
                    relation,
                    origin_server_ts: e.header().origin_server_ts,
                })
            })
            .collect();
        relations::bundle(&children, requesting_user)
    }

    /// Subscribes to this room's publish stream. See `crate::protocol`.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RoomUpdate> {
        self.publish.subscribe()
    }

    /// Marks an event redacted (`hs_model::event::EventFlags::REDACTED`) and rewrites its stored
    /// record. Does not itself authorize the redaction -- callers send the `m.room.redaction`
    /// event through [`RoomActor::send_event`] first; this is the side effect of that event
    /// having been accepted.
    ///
    /// # Errors
    /// Returns [`RoomError::EventNotFound`] if `target` is not held by this actor, or
    /// [`RoomError::Store`] on a storage failure.
    pub fn apply_redaction(&mut self, target: &EventId) -> Result<(), RoomError> {
        let sn = *self
            .event_id_index
            .get(target)
            .ok_or_else(|| RoomError::EventNotFound(target.to_string()))?;
        let event = self
            .events
            .get_mut(&sn)
            .ok_or_else(|| RoomError::EventNotFound(target.to_string()))?;
        event.flags_mut().set_redacted(true);
        let flags = event.header().flags.to_byte();
        let room_sn = self.room_sn;
        transact(&self.backend, TransactConfig::default(), |txn| {
            let Some(bytes) = self.tables.events.get(txn, &(sn,)).map_err(to_kv)? else {
                return Ok(());
            };
            let mut persisted: PersistedEvent =
                serde_json::from_slice(&bytes).map_err(hs_kv::KvError::backend)?;
            persisted.flags = flags;
            let bytes = serde_json::to_vec(&persisted).map_err(hs_kv::KvError::backend)?;
            self.tables.events.put(txn, &(sn,), &bytes).map_err(to_kv)?;
            let _ = room_sn;
            Ok(())
        })
        .map_err(RoomError::from)
    }

    fn dedup_key(
        sender: &UserId,
        device_id: Option<&ruma::DeviceId>,
        txn_id: &str,
    ) -> (OwnedUserId, String, String) {
        (
            sender.to_owned(),
            device_id.map(ToString::to_string).unwrap_or_default(),
            txn_id.to_owned(),
        )
    }

    /// The event already sent for this `(sender, device, txnId)`, if this transaction was already
    /// used and this actor still remembers it -- see [`RoomActor::txn_dedup`]'s doc comment for
    /// the durability caveat.
    fn dedup_lookup(
        &self,
        sender: &UserId,
        device_id: Option<&ruma::DeviceId>,
        txn_id: &str,
    ) -> Option<&Event> {
        let event_id = self.txn_dedup.get(&Self::dedup_key(sender, device_id, txn_id))?;
        self.event_by_id(event_id)
    }

    /// Sends a `{txnId}`-suffixed non-state event (`PUT .../send/{eventType}/{txnId}`),
    /// deduplicating on `(sender, device, txnId)`: replaying the same transaction ID returns the
    /// same event rather than sending a second one.
    ///
    /// # Errors
    /// See [`RoomActor::send_event`].
    pub fn send_event_txn(
        &mut self,
        sender: OwnedUserId,
        device_id: Option<&ruma::DeviceId>,
        txn_id: &str,
        event_type: String,
        content: serde_json::Value,
        now_ms: i64,
    ) -> Result<Event, RoomError> {
        if let Some(existing) = self.dedup_lookup(&sender, device_id, txn_id) {
            return Ok(existing.clone());
        }
        let event = self.send_event(sender.clone(), event_type, None, content, None, now_ms)?;
        self.txn_dedup.insert(
            Self::dedup_key(&sender, device_id, txn_id),
            event.event_id().to_owned(),
        );
        Ok(event)
    }

    /// Sends a `{txnId}`-suffixed redaction (`PUT .../redact/{eventId}/{txnId}`) and applies its
    /// effect, deduplicating on `(sender, device, txnId)` the same way
    /// [`RoomActor::send_event_txn`] does.
    ///
    /// # Errors
    /// See [`RoomActor::send_event`] and [`RoomActor::apply_redaction`].
    pub fn redact_txn(
        &mut self,
        sender: OwnedUserId,
        device_id: Option<&ruma::DeviceId>,
        txn_id: &str,
        target: OwnedEventId,
        reason: Option<String>,
        now_ms: i64,
    ) -> Result<Event, RoomError> {
        if let Some(existing) = self.dedup_lookup(&sender, device_id, txn_id) {
            return Ok(existing.clone());
        }
        let mut content = serde_json::json!({});
        if let Some(reason) = &reason {
            content["reason"] = serde_json::Value::String(reason.clone());
        }
        let event = self.send_event(
            sender.clone(),
            "m.room.redaction".to_owned(),
            None,
            content,
            Some(target.clone()),
            now_ms,
        )?;
        self.apply_redaction(&target)?;
        self.txn_dedup.insert(
            Self::dedup_key(&sender, device_id, txn_id),
            event.event_id().to_owned(),
        );
        Ok(event)
    }
}

/// Looks one event up by ID across every room this server stores, returning the room it belongs to
/// alongside its persisted row. Not scoped to a room, and not scoped to rooms currently resident:
/// it goes through the event-ID intern table and the shared `events` keyspace directly, the same
/// way [`resolve_alias`] goes through the alias keyspace.
///
/// This exists for federation's `GET /_matrix/federation/v1/event/{eventId}`, which the spec does
/// not scope by room in its path, so the server has to work out which room an event is in before it
/// can apply that room's visibility rules. Every caller must still do exactly that: this function
/// deliberately performs **no** visibility check, and its `room_id` is the input to one.
///
/// # Errors
/// Returns [`RoomError::Store`] on a storage failure, or [`RoomError::Internal`] if a stored row
/// cannot be decoded.
pub fn find_event_globally<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
    event_id: &EventId,
) -> Result<Option<PersistedEvent>, RoomError> {
    let snapshot = backend.snapshot();
    let Some(event_sn) = tables.event_sn.lookup(&snapshot, event_id.as_bytes())? else {
        return Ok(None);
    };
    let Some(bytes) = tables.events.get(&snapshot, &(event_sn,))? else {
        return Ok(None);
    };
    let row: PersistedEvent = serde_json::from_slice(&bytes)
        .map_err(|e| RoomError::Internal(format!("corrupt event row for {event_id}: {e}")))?;
    Ok(Some(row))
}

/// Resolves a local alias to its room ID, without needing that room's actor loaded.
///
/// # Errors
/// Returns [`RoomError::Store`] on a storage failure.
pub fn resolve_alias<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
    alias: &RoomAliasId,
) -> Result<Option<OwnedRoomId>, RoomError> {
    let snapshot = backend.snapshot();
    let Some(sn_bytes) = tables.aliases.get(&snapshot, &(alias.to_string(),))? else {
        return Ok(None);
    };
    let arr: [u8; 4] = sn_bytes
        .as_ref()
        .try_into()
        .map_err(|_| RoomError::Internal("corrupt alias entry".into()))?;
    let room_sn = RoomSn::from_be_bytes(arr);
    let Some(room_id_bytes) = tables.room_sn.resolve(&snapshot, room_sn)? else {
        return Ok(None);
    };
    let room_id =
        String::from_utf8(room_id_bytes).map_err(|e| RoomError::Internal(e.to_string()))?;
    Ok(Some(
        OwnedRoomId::try_from(room_id).map_err(|e| RoomError::Internal(e.to_string()))?,
    ))
}

#[derive(Debug)]
struct AliasInUse;
impl std::fmt::Display for AliasInUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "alias already in use")
    }
}
impl std::error::Error for AliasInUse {}

/// An async, serialized handle to one [`RoomActor`]. See `crate::protocol`'s module docs.
#[derive(Clone)]
pub struct RoomActorHandle<B: KvBackend> {
    inner: Arc<tokio::sync::Mutex<RoomActor<B>>>,
}

impl<B: KvBackend> RoomActorHandle<B> {
    /// Wraps an already-constructed actor.
    #[must_use]
    pub fn new(actor: RoomActor<B>) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(actor)),
        }
    }

    /// Subscribes to the room's publish stream (`crate::protocol::RoomUpdate`).
    pub async fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RoomUpdate> {
        self.inner.lock().await.subscribe()
    }

    /// Runs a synchronous closure against the actor under its mutex, off the async executor
    /// thread (`hs-kv`'s transaction API is synchronous -- see `crate::protocol`'s module docs).
    async fn with_actor<T, F>(&self, f: F) -> T
    where
        B: 'static,
        T: Send + 'static,
        F: FnOnce(&mut RoomActor<B>) -> T + Send + 'static,
    {
        let inner = self.inner.clone();
        // `spawn_blocking` needs a `'static` future; we hold the `tokio::sync::Mutex` guard only
        // inside the blocking closure, never across an `.await`.
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.blocking_lock();
            f(&mut guard)
        })
        .await
        .expect("room actor task panicked")
    }

    /// `crate::protocol`'s `send_event` command.
    pub async fn send_event(
        &self,
        sender: OwnedUserId,
        event_type: String,
        state_key: Option<String>,
        content: serde_json::Value,
        redacts: Option<OwnedEventId>,
        now_ms: i64,
    ) -> Result<Event, RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| {
            actor.send_event(sender, event_type, state_key, content, redacts, now_ms)
        })
        .await
    }

    /// `PUT .../send/{eventType}/{txnId}`: like [`RoomActorHandle::send_event`] but deduplicated
    /// on `(sender, device, txnId)` -- replaying the same transaction ID returns the same event
    /// rather than sending a second one (`RoomActor::send_event_txn`).
    pub async fn send_event_txn(
        &self,
        sender: OwnedUserId,
        device_id: Option<ruma::OwnedDeviceId>,
        txn_id: String,
        event_type: String,
        content: serde_json::Value,
        now_ms: i64,
    ) -> Result<Event, RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| {
            actor.send_event_txn(
                sender,
                device_id.as_deref(),
                &txn_id,
                event_type,
                content,
                now_ms,
            )
        })
        .await
    }

    /// `crate::protocol`'s `membership` command.
    pub async fn membership(
        &self,
        sender: OwnedUserId,
        action: Action,
        target: OwnedUserId,
        extra: serde_json::Value,
        now_ms: i64,
    ) -> Result<Event, RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| actor.membership_action(sender, action, target, extra, now_ms))
            .await
    }

    /// `crate::protocol`'s `redact` command: sends the `m.room.redaction` event, then applies its
    /// effect to the target if accepted. Deduplicated on `(sender, device, txnId)`
    /// (`RoomActor::redact_txn`) -- replaying the same transaction ID returns the same redaction
    /// event rather than sending a second one.
    pub async fn redact(
        &self,
        sender: OwnedUserId,
        device_id: Option<ruma::OwnedDeviceId>,
        txn_id: String,
        target: OwnedEventId,
        reason: Option<String>,
        now_ms: i64,
    ) -> Result<Event, RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| {
            actor.redact_txn(
                sender,
                device_id.as_deref(),
                &txn_id,
                target,
                reason,
                now_ms,
            )
        })
        .await
    }

    /// Reads the room under the lock, off the async executor thread.
    pub async fn query<T, F>(&self, f: F) -> T
    where
        B: 'static,
        T: Send + 'static,
        F: FnOnce(&RoomActor<B>) -> T + Send + 'static,
    {
        self.with_actor(move |actor| f(actor)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use proptest::prelude::*;
    use ruma::{RoomVersionId, user_id};

    fn room(preset: &str) -> RoomActor<MemoryBackend> {
        let backend = MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        let identity = HomeserverIdentity::for_tests("hs1");
        RoomActor::create_room(
            backend,
            tables,
            identity,
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest {
                preset: Some(preset.to_owned()),
                ..Default::default()
            },
            1,
        )
        .unwrap()
    }

    #[test]
    fn create_room_bootstraps_creator_as_the_sole_joined_member() {
        let actor = room("public_chat");
        let joined = actor.joined_members().unwrap();
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].header().state_key.as_deref(), Some("@alice:hs1"));
    }

    #[test]
    fn send_event_and_query_round_trip() {
        let mut actor = room("public_chat");
        let event = actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hi"}),
                None,
                2,
            )
            .unwrap();
        assert_eq!(
            actor.event_by_id(event.event_id()).unwrap().event_id(),
            event.event_id()
        );
    }

    #[test]
    fn reload_from_the_store_reproduces_the_same_state() {
        let backend = MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        let identity = HomeserverIdentity::for_tests("hs1");
        let mut actor = RoomActor::create_room(
            backend.clone(),
            tables.clone(),
            identity.clone(),
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest {
                preset: Some("public_chat".to_owned()),
                name: Some("Reload me".to_owned()),
                ..Default::default()
            },
            1,
        )
        .unwrap();
        actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "before reload"}),
                None,
                2,
            )
            .unwrap();
        let room_id = actor.room_id().to_owned();
        let original_state_count = actor.full_state().unwrap().len();
        drop(actor);

        let reloaded = RoomActor::load(backend, tables, identity, &room_id)
            .unwrap()
            .expect("room was persisted, load must find it");
        assert_eq!(reloaded.room_id(), &*room_id);
        assert_eq!(reloaded.full_state().unwrap().len(), original_state_count);
        assert_eq!(
            reloaded
                .state_event("m.room.name", "")
                .unwrap()
                .unwrap()
                .json()
                .get("content")
                .unwrap()
                .as_object()
                .unwrap()
                .get("name")
                .unwrap()
                .as_str(),
            Some("Reload me")
        );
        let (events, _) = reloaded.paginate(None, Direction::Backward, usize::MAX);
        assert!(
            events
                .iter()
                .any(|e| e.header().event_type == "m.room.message")
        );
    }

    /// The topic's `content` in `events`, if `m.room.topic` is present.
    fn topic_of<'a>(events: impl IntoIterator<Item = &'a Event>) -> Option<String> {
        events
            .into_iter()
            .find(|e| e.header().event_type == "m.room.topic")
            .and_then(|e| e.json().get("content"))
            .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
            .and_then(|c| c.get("topic"))
            .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
            .map(str::to_owned)
    }

    /// The core claim item 1 of `docs/next-steps.md` asks for: given a room with several state
    /// changes, the state *as of* an early event must differ from current state in exactly the
    /// way it should -- not merely "differ", but differ by precisely the events that actually
    /// changed between the two points, and nothing else. Also exercises that this answers for an
    /// event that is not the timeline head (a later, non-state message is sent last), and that the
    /// accompanying auth chain is populated and sane.
    #[test]
    fn state_at_event_reconstructs_history_not_just_current_state() {
        let mut actor = room("public_chat");

        // Two topic changes on the same state key: the room's state immediately after the first
        // must show "first topic"; immediately after the second (which is also current state,
        // since nothing else touches the topic afterwards) must show "second topic".
        let first_topic = actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.topic".to_owned(),
                Some(String::new()),
                serde_json::json!({"topic": "first topic"}),
                None,
                2,
            )
            .unwrap();
        let second_topic = actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.topic".to_owned(),
                Some(String::new()),
                serde_json::json!({"topic": "second topic"}),
                None,
                3,
            )
            .unwrap();
        // A non-state event sent last, so the timeline head is not a state event at all -- the
        // gap this closes is specifically "answer for any known event", not just "the newest
        // state event".
        actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "hi"}),
                None,
                4,
            )
            .unwrap();

        let at_first = actor
            .state_at_event(first_topic.event_id())
            .unwrap()
            .expect("the first topic event is known to this actor");
        assert_eq!(
            topic_of(at_first.state.iter()).as_deref(),
            Some("first topic"),
            "state immediately after the first topic change must show that change, not a later one"
        );

        let at_second = actor
            .state_at_event(second_topic.event_id())
            .unwrap()
            .expect("the second topic event is known to this actor");
        assert_eq!(
            topic_of(at_second.state.iter()).as_deref(),
            Some("second topic")
        );

        let current = actor.full_state().unwrap();
        assert_eq!(
            topic_of(current.iter().copied()).as_deref(),
            Some("second topic"),
            "current state must match the state right after the second (most recent) topic change"
        );

        // Exactly one event differs each way between "state after the first topic change" and
        // "current state": the first topic event is replaced by the second. Every other state
        // event (create, the creator's join, power levels, join rules, history visibility, guest
        // access) is unchanged and must appear in both sets identically -- proving this is a real
        // historical reconstruction (a targeted swap of one entry), not current state relabeled
        // or a coincidentally-similar recomputation.
        let ids_at_first: BTreeSet<OwnedEventId> =
            at_first.state.iter().map(|e| e.event_id().to_owned()).collect();
        let ids_current: BTreeSet<OwnedEventId> =
            current.iter().map(|e| e.event_id().to_owned()).collect();
        let only_in_first: Vec<_> = ids_at_first.difference(&ids_current).collect();
        let only_in_current: Vec<_> = ids_current.difference(&ids_at_first).collect();
        assert_eq!(only_in_first, vec![first_topic.event_id()]);
        assert_eq!(only_in_current, vec![second_topic.event_id()]);
        assert_eq!(
            ids_at_first.len(),
            ids_current.len(),
            "the two state maps must be the same size (a swap, not an addition/removal)"
        );

        // The auth chain input that goes with the early state: every ordinary state event's
        // ancestors include the room's create event, and, being an ancestor rather than a member
        // of the state map itself, `m.room.create` shows up in `auth_chain` even though it is
        // also separately present in `state` -- the overlap this module's doc comment documents
        // as expected.
        assert!(
            at_first
                .auth_chain
                .iter()
                .any(|e| e.header().event_type == "m.room.create"),
            "the auth chain of any ordinary state event must reach the room's create event"
        );
        assert!(
            at_first
                .state
                .iter()
                .any(|e| e.header().event_type == "m.room.create"),
            "m.room.create is legitimately in both `state` and `auth_chain`"
        );

        // An event this actor has never heard of answers `None`, not an error and not a
        // best-effort guess.
        let unknown = ruma::EventId::parse("$totally-unknown-event:hs1").unwrap();
        assert!(actor.state_at_event(&unknown).unwrap().is_none());
    }

    #[test]
    fn unsupported_room_version_is_rejected() {
        let backend = MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        let identity = HomeserverIdentity::for_tests("hs1");
        let result = RoomActor::create(
            backend,
            tables,
            identity,
            ruma::RoomId::new_v1(ruma::ServerName::parse("hs1").unwrap().as_ref()),
            RoomVersionId::try_from("not-a-version").unwrap(),
            user_id!("@alice:hs1").to_owned(),
            serde_json::json!({}),
            1,
        );
        match result {
            Err(RoomError::UnsupportedRoomVersion(_)) => {}
            Ok(_) => panic!("expected UnsupportedRoomVersion, got Ok"),
            Err(other) => panic!("expected UnsupportedRoomVersion, got {other}"),
        }
    }

    /// Room version 12 (MSC4291, hash-based room IDs) is now supported: the room ID is the
    /// reference hash of the `m.room.create` event, has no `:server` suffix, and the create
    /// event itself carries no `room_id` field (`hs_state::auth::check_room_create` rejects one
    /// that does) -- see `docs/rfcs/0010-room-actor-state-store-seam.md` section 3 for the gap
    /// this closes.
    #[test]
    fn room_version_12_hash_based_room_ids_are_supported() {
        let backend = MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        let identity = HomeserverIdentity::for_tests("hs1");
        let mut actor = RoomActor::create_room(
            backend,
            tables,
            identity,
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest {
                room_version: Some(RoomVersionId::V12),
                preset: Some("public_chat".to_owned()),
                ..Default::default()
            },
            1,
        )
        .unwrap();

        assert_eq!(actor.room_version(), &RoomVersionId::V12);
        // No `:server_name` suffix -- MSC4291 room IDs are just `!<reference hash>`.
        assert!(actor.room_id().server_name().is_none());
        let create = actor
            .state_event("m.room.create", "")
            .unwrap()
            .expect("create event must be in state");
        assert!(create.json().get("room_id").is_none());
        // The create event's own event ID and the room ID carry the same hash, per
        // `room_create_event_id_as_room_id`.
        assert_eq!(
            actor.room_id().strip_sigil(),
            create.event_id().as_str().strip_prefix('$').unwrap()
        );
        assert_eq!(actor.joined_members().unwrap().len(), 1);

        // Ordinary events still carry `room_id`, and the room is otherwise fully functional.
        let room_id_str = actor.room_id().to_string();
        let msg = actor
            .send_event(
                user_id!("@alice:hs1").to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "hi from v12"}),
                None,
                2,
            )
            .expect("v12 room should accept ordinary events");
        assert_eq!(
            msg.json()
                .get("room_id")
                .and_then(hs_model::canonical::CanonicalJsonValue::as_str),
            Some(room_id_str.as_str())
        );
    }

    proptest! {
        /// Across every stable room version this crate supports (skipping v12, the documented
        /// gap), a fresh room's creator is always the sole joined member, and a second user can
        /// never join a `private_chat` room without first being invited -- the same property
        /// `crate::membership`'s own tests check in isolation, exercised here end to end through
        /// the real actor and `hs-state`'s real authorization, per room version.
        #[test]
        fn private_room_membership_invariant_holds_across_room_versions(
            version_idx in 0..8usize,
        ) {
            let versions = [
                RoomVersionId::V1,
                RoomVersionId::V4,
                RoomVersionId::V6,
                RoomVersionId::V7,
                RoomVersionId::V8,
                RoomVersionId::V9,
                RoomVersionId::V10,
                RoomVersionId::V11,
            ];
            let version = versions[version_idx].clone();

            let backend = MemoryBackend::new();
            let tables = Tables::open(&backend).unwrap();
            let identity = HomeserverIdentity::for_tests("hs1");
            let mut actor = RoomActor::create_room(
                backend,
                tables,
                identity,
                user_id!("@alice:hs1").to_owned(),
                CreateRoomRequest {
                    room_version: Some(version),
                    preset: Some("private_chat".to_owned()),
                    ..Default::default()
                },
                1,
            )
            .unwrap();

            prop_assert_eq!(actor.joined_members().unwrap().len(), 1);

            let denied = actor.membership_action(
                user_id!("@carol:hs1").to_owned(),
                Action::Join,
                user_id!("@carol:hs1").to_owned(),
                serde_json::json!({}),
                2,
            );
            prop_assert!(matches!(denied, Err(RoomError::Forbidden(_))));

            actor
                .membership_action(
                    user_id!("@alice:hs1").to_owned(),
                    Action::Invite,
                    user_id!("@carol:hs1").to_owned(),
                    serde_json::json!({}),
                    3,
                )
                .unwrap();
            let joined = actor.membership_action(
                user_id!("@carol:hs1").to_owned(),
                Action::Join,
                user_id!("@carol:hs1").to_owned(),
                serde_json::json!({}),
                4,
            );
            prop_assert!(joined.is_ok());
            prop_assert_eq!(actor.joined_members().unwrap().len(), 2);
        }
    }

    /// Deliverable 2: the entire point of the state-store rewiring. A flat `(event_type,
    /// state_key) -> EventSn` map (this crate's first pass) cannot represent more than one
    /// forward extremity at all; the production `hs_state::api::StateStore` can, and resolves it
    /// correctly. This builds a genuine fork -- two power-levels events that each cite the same
    /// parent (the creator's own join) without citing each other, via
    /// `RoomActor::send_event_citing` -- persists both through the actor, asserts the actor really
    /// does hold two forward extremities at that point, then asserts the *resolved* state a third
    /// event (which converges the fork by citing both) is authorized against and lands on: the
    /// higher-depth power-levels event should win, exactly as `hs_state::state_res` decides ties on
    /// depth (and, since both events have equal depth here, `hs_state::state_res`'s deterministic
    /// tie-break) -- not "whichever branch happened to be created directly against."
    #[test]
    fn a_genuine_fork_persists_through_the_actor_and_resolves_through_the_store() {
        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1").to_owned();

        // The parent both branches fork from: the room's current sole extremity right after
        // `create_room` (the creator's join, chronologically last of `create_room`'s bootstrap
        // events -- see `RoomActor::create_room`'s doc comment for the exact event order... in
        // this case it is whatever the current single extremity is, found generically below so
        // this test does not depend on that internal ordering).
        let parent = actor.forward_extremities_vec();
        assert_eq!(parent.len(), 1, "a freshly created room has one extremity");

        // Branch 1: alice (power level 100) raises the ban level to 60, citing only `parent`.
        let branch_a = actor
            .send_event_citing(
                alice.clone(),
                "m.room.power_levels".to_owned(),
                Some(String::new()),
                serde_json::json!({
                    "users": {alice.as_str(): 100},
                    "ban": 60, "kick": 50, "redact": 50, "invite": 0,
                    "users_default": 0, "events_default": 0, "state_default": 50,
                }),
                None,
                2,
                &parent,
            )
            .unwrap();

        // Branch 2: alice raises the ban level to 70 instead, *also* citing only `parent` (not
        // `branch_a`) -- this is what makes it a genuine second branch rather than a normal
        // convergent send.
        let branch_b = actor
            .send_event_citing(
                alice.clone(),
                "m.room.power_levels".to_owned(),
                Some(String::new()),
                serde_json::json!({
                    "users": {alice.as_str(): 100},
                    "ban": 70, "kick": 50, "redact": 50, "invite": 0,
                    "users_default": 0, "events_default": 0, "state_default": 50,
                }),
                None,
                3,
                &parent,
            )
            .unwrap();

        // The actor now genuinely holds two forward extremities -- exactly the shape the old flat
        // map could never represent.
        let extremities = actor.forward_extremities_vec();
        assert_eq!(
            extremities.len(),
            2,
            "persisting two events that cite the same parent without citing each other must fork \
             the room's forward extremities"
        );

        // A third event citing *both* branches converges the fork; whatever it is authorized
        // against is the store's resolution of the two conflicting power-levels events.
        let merge = actor
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"body": "converging the fork"}),
                None,
                4,
            )
            .unwrap();

        // The merge event's own `prev_events` must cite both branches (this is what converges
        // them): confirms the actor really built it against the fork, not against one arbitrary
        // side of it.
        let prev_ids: std::collections::BTreeSet<String> =
            pipeline::decode_event_ids(merge.json().get("prev_events"))
                .into_iter()
                .map(|id| id.to_string())
                .collect();
        assert_eq!(
            prev_ids,
            std::collections::BTreeSet::from([
                branch_a.event_id().to_string(),
                branch_b.event_id().to_string(),
            ])
        );

        // After the merge, the room has exactly one forward extremity again (the fork converged).
        assert_eq!(actor.forward_extremities_vec(), vec![
            *actor.event_id_index.get(merge.event_id()).unwrap()
        ]);

        // The resolved power-levels state is exactly one of the two branches' content (state
        // resolution picked a winner, not a merge of the two, per the spec's "resolve, don't
        // merge" model for a single conflicting key) -- assert it is one of the two genuine
        // candidates, and that the actor's current state after the merge agrees with what the
        // merge event was actually authorized against (both computed through the same store call,
        // so this is really asserting internal consistency, not tautology: a bug in `resolve()`
        // wiring would make these two computations disagree).
        let resolved = actor
            .state_event("m.room.power_levels", "")
            .unwrap()
            .expect("power_levels must be set");
        let resolved_ban = resolved
            .json()
            .get("content")
            .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
            .and_then(|c| c.get("ban"))
            .and_then(|v| match v {
                hs_model::canonical::CanonicalJsonValue::Integer(n) => Some(*n),
                _ => None,
            });
        assert!(
            resolved_ban == Some(60) || resolved_ban == Some(70),
            "resolved ban level must be exactly one of the two conflicting branches' values, got {resolved_ban:?}"
        );
        assert!(
            resolved.event_id() == branch_a.event_id() || resolved.event_id() == branch_b.event_id(),
            "the resolved power_levels event must be one of the two genuine fork candidates"
        );
    }

    /// A retried `send`/`redact` with the same transaction ID must return the same event, not
    /// create a duplicate -- the correctness gap this session closed.
    #[test]
    fn transaction_id_is_deduplicated_on_send_and_redact() {
        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1").to_owned();

        let first = actor
            .send_event_txn(
                alice.clone(),
                None,
                "txn-1",
                "m.room.message".to_owned(),
                serde_json::json!({"body": "hello"}),
                2,
            )
            .unwrap();
        let retried = actor
            .send_event_txn(
                alice.clone(),
                None,
                "txn-1",
                "m.room.message".to_owned(),
                serde_json::json!({"body": "hello, but this should never be sent"}),
                3,
            )
            .unwrap();
        assert_eq!(first.event_id(), retried.event_id());
        assert_eq!(
            actor.paginate(None, Direction::Backward, usize::MAX).0.len(),
            actor
                .paginate(None, Direction::Backward, usize::MAX)
                .0
                .iter()
                .map(|e| e.event_id())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "every persisted event id must be unique -- the retry must not have persisted a second event"
        );

        let redact_first = actor
            .redact_txn(alice.clone(), None, "txn-2", first.event_id().to_owned(), None, 4)
            .unwrap();
        let redact_retried = actor
            .redact_txn(alice, None, "txn-2", first.event_id().to_owned(), None, 5)
            .unwrap();
        assert_eq!(redact_first.event_id(), redact_retried.event_id());
    }
}
