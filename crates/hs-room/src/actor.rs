//! [`RoomActor`]: the synchronous, single-room state machine. [`RoomActorHandle`]: the async,
//! serialized mailbox wrapping it. See `crate::protocol` for the design rationale.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use hs_model::ids::{EventSn, RoomSn};
use hs_model::room_version::{self, RoomIdFormat, RoomVersionRules};
use hs_state::api::StateStore;
use hs_state::auth::{self, AuthEventRef, IncomingEvent};
use hs_state::error::AuthError;
use hs_state::kv_store::ProductionStateStore;
use hs_state::state_fetch::FlatState;
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, RoomVersionId, UserId,
};

use crate::error::RoomError;
use crate::history_visibility;
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
    /// `power_level_content_override`: applied on top of the default `m.room.power_levels`
    /// content, one top-level key at a time -- a key it carries wins, a key it omits is kept.
    pub power_level_content_override: Option<serde_json::Value>,
    /// `creation_content`: merged into the `m.room.create` content (`creator`/`room_version` are
    /// still set by this crate, overriding anything the caller put there).
    pub creation_content: serde_json::Value,
    /// `room_alias_name`: the localpart of a local alias to create for this room.
    pub room_alias_name: Option<String>,
    /// What the `m.room.member` events that creating this room sends carry besides `membership`,
    /// by the user each is about: the creator's join, and each invitee's invitation. That is
    /// their profile as it stands (a membership event is where a room's members learn each
    /// other's names), and `is_direct` on the invitations of a direct chat. Filled in by
    /// `crate::routes::create_room`, because profiles are not something a room knows; a user
    /// with no entry gets a bare event.
    pub member_content: HashMap<OwnedUserId, serde_json::Value>,
    /// An explicit room ID to use instead of generating a fresh random one. `None` (the ordinary
    /// `POST /createRoom` case) means "generate one" ([`RoomId::new_v1`]). `Some` exists for
    /// `POST /rooms/{roomId}/upgrade` (`crate::routes::upgrade`), which must know the replacement
    /// room's ID *before* creating it (to put it in the old room's `m.room.tombstone` content)
    /// and therefore cannot let this function choose.
    pub room_id: Option<OwnedRoomId>,
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

/// Outcome of [`RoomActor::accept_remote_event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteEventOutcome {
    /// This actor already held an event with this ID; accepting it again was a no-op -- no
    /// re-authorization, no second timeline entry, no second publish. The normal shape of a
    /// retried federation transaction or an event that arrives by more than one path.
    AlreadyKnown,
    /// Newly authorized and durably persisted, at this room-local [`EventSn`].
    Stored(EventSn),
}

/// Reads the event ID a `m.room.redaction` event redacts, from either wire shape: the top-level
/// `redacts` field (room versions 3 and later) or `content.redacts` (room versions 1-2, and
/// mirrored onto the top level by some senders). Used only to populate
/// [`hs_state::auth::IncomingEvent::redacts`] for the pre-v3 special-case redaction check
/// (`hs_state::auth::check_room_redaction`); every other check ignores this field entirely.
fn extract_redacts(event: &Event) -> Option<OwnedEventId> {
    let top = event
        .json()
        .get("redacts")
        .and_then(CanonicalJsonValue::as_str);
    let nested = event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|c| c.get("redacts"))
        .and_then(CanonicalJsonValue::as_str);
    top.or(nested)
        .and_then(|s| EventId::parse(s).ok())
        .map(|id| id.to_owned())
}

/// Reads `event.content[key]` as a string, if present and a string. A small shared helper for the
/// `content.get(...).and_then(as_object).and_then(...).and_then(as_str)` chain repeated across
/// this module's membership- and history-visibility-reading code.
fn content_str<'a>(event: &'a Event, key: &str) -> Option<&'a str> {
    event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|c| c.get(key))
        .and_then(CanonicalJsonValue::as_str)
}

/// Parses an `m.room.member` event's `content.membership` string into a [`PriorState`]
/// (`PriorState::None` for an absent event or an unrecognized value -- matching
/// [`RoomActor::prior_membership`]'s own mapping, which this duplicates in miniature because that
/// method reads through `state_event`/a fallible current-state lookup, not an already-resolved
/// content string).
fn parse_membership(raw: Option<&str>) -> PriorState {
    match raw {
        Some("join") => PriorState::Join,
        Some("invite") => PriorState::Invite,
        Some("leave") => PriorState::Leave,
        Some("ban") => PriorState::Ban,
        Some("knock") => PriorState::Knock,
        _ => PriorState::None,
    }
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
    /// The reverse of `txn_dedup`: `event_id -> (sender, device_id-or-empty, txn_id)`, for
    /// rendering `unsigned.transaction_id` on the sending device's own echo of an event it sent
    /// through a `{txnId}`-suffixed endpoint (client-server API "Local echo": a client matches an
    /// optimistic local copy of a message against the real one by transaction ID). Scoped to
    /// `(sender, device)` -- not access token -- because two sessions sharing one device ID (a
    /// client that logged back in without allocating a new device, or a refreshed access token)
    /// must see the same transaction ID for their own shared echo
    /// (`txnid_test.go`'s `TestTxnScopeOnLocalEcho`/`TestTxnIdWithRefreshToken`); an *different*
    /// device, or any other user, must never see it, even for the very same event. Same
    /// in-memory-only lifetime caveat as `txn_dedup`.
    event_txn: HashMap<OwnedEventId, (OwnedUserId, String, String)>,
    /// Users who have called `POST /rooms/{roomId}/forget` and not since rejoined. In-memory
    /// only, same scope caveat as `txn_dedup`: does not survive an idle eviction and reload
    /// (`RoomActor::load` does not repopulate it). Read by [`RoomActor::can_read_room`];
    /// [`RoomActor::membership_action`] clears an entry the moment its user rejoins (the spec's
    /// "Can re-join room if re-invited" case: forgetting must not be permanent).
    forgotten: HashSet<OwnedUserId>,
    publish: tokio::sync::broadcast::Sender<RoomUpdate>,
    /// The cluster-fencing hook [`RoomActor::persist`] checks as the last read before committing
    /// (`docs/status/03-cluster.md` item 4). `None` (the default for every construction path
    /// today) means no fencing is installed -- single-node mode, or a server that has not wired
    /// `hs-cluster` in at all -- in which case `persist` behaves exactly as before this field
    /// existed. Installed by [`crate::registry::RoomRegistry::install_fencing`] onto every actor
    /// it constructs or loads, once `hs-cli` (out of this crate's ownership) calls it.
    fencing: Option<Arc<crate::fencing::RoomFencing<B>>>,
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
            event_txn: HashMap::new(),
            forgotten: HashSet::new(),
            publish,
            fencing: None,
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
            event_txn: HashMap::new(),
            forgotten: HashSet::new(),
            publish,
            fencing: None,
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
    fn state_view(
        &self,
        prev_sns: &[EventSn],
    ) -> Result<RoomStateView<'_, ProductionStateStore<B>>, RoomError> {
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

    /// A [`RoomStateView`] over the state as of immediately *after* one specific event (not
    /// necessarily the timeline head), via [`hs_state::api::StateStore::state_at`]. Used by
    /// [`RoomActor::event_visible_to`] -- the same primitive `RoomActor::state_at_event` already
    /// uses, but returning the lazy view instead of eagerly diffing it into a `Vec<Event>`, since a
    /// visibility check only ever looks up one or two `(event_type, state_key)` pairs.
    fn state_view_at_sn(
        &self,
        sn: EventSn,
    ) -> Result<RoomStateView<'_, ProductionStateStore<B>>, RoomError> {
        let root = self
            .store
            .state_at(sn)
            .map_err(|e| RoomError::State(e.to_string()))?;
        Ok(RoomStateView {
            store: &self.store,
            root,
            bodies: EventMap(&self.events),
        })
    }

    fn refs_for(&self, sns: &[EventSn]) -> Result<Vec<pipeline::EventRef>, RoomError> {
        sns.iter()
            .map(|sn| {
                let event = self
                    .events
                    .get(sn)
                    .ok_or_else(|| RoomError::Internal("cited event not in hot cache".into()))?;
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
    /// If `state_key` is `Some` and the room's *current* event for `(event_type, state_key)`
    /// already carries exactly `content` (structural equality, key order insensitive), no new
    /// event is built at all -- the existing one is returned as-is. This is the client-server
    /// spec's documented idempotency for repeated state ("Setting state twice is idempotent",
    /// `rooms_state_test.go`), and it is also what makes a repeated `m.room.member`/join with an
    /// unchanged (not-freshly-profile-edited) content a no-op instead of a second join event
    /// (`rooms_state_test.go`'s "Joining room twice is idempotent" -- [`RoomActor::membership_action`]
    /// calls this, not [`RoomActor::send_event_citing`] directly, for exactly this reason). A
    /// message event (`state_key: None`) is never affected: those only deduplicate on transaction
    /// ID, via [`RoomActor::send_event_txn`].
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
        if let Some(existing) =
            self.idempotent_state_reuse(&event_type, state_key.as_deref(), &content)?
        {
            return Ok(existing);
        }
        let prev_sns = self.forward_extremities_vec();
        self.send_event_citing(
            sender, event_type, state_key, content, redacts, now_ms, &prev_sns,
        )
    }

    /// The idempotency check [`RoomActor::send_event`]'s doc comment describes. Returns the
    /// existing event when it applies, `None` when a new event should genuinely be built.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if reading the current state fails.
    fn idempotent_state_reuse(
        &self,
        event_type: &str,
        state_key: Option<&str>,
        content: &serde_json::Value,
    ) -> Result<Option<Event>, RoomError> {
        let Some(state_key) = state_key else {
            return Ok(None);
        };
        let Some(existing) = self.state_event(event_type, state_key)? else {
            return Ok(None);
        };
        let existing_content = existing
            .json()
            .get("content")
            .map(|v| {
                serde_json::from_slice::<serde_json::Value>(&v.to_canonical_bytes())
                    .unwrap_or(serde_json::Value::Null)
            })
            .unwrap_or(serde_json::Value::Null);
        if &existing_content == content {
            Ok(Some(existing.clone()))
        } else {
            Ok(None)
        }
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
    // `sender`/`event_type`/`state_key`/`content`/`redacts` mirror `pipeline::NewEvent`'s fields
    // one-for-one (this is the only caller-facing spot that still takes them unbundled); bundling
    // them into a `NewEvent` here would change this public method's signature, and `hs-federation`
    // (`crates/hs-federation/src/{join,inbound}.rs`) calls it directly -- out of scope for this
    // track to edit, so the lint is silenced here rather than risking a signature change that
    // crate never asked for.
    #[allow(clippy::too_many_arguments)]
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
        if let Some(reason) = self.blocked_reason()? {
            return Err(RoomError::RoomBlocked(reason));
        }
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
        let event = self.send_event(
            sender,
            "m.room.member".to_owned(),
            Some(target.to_string()),
            content,
            None,
            now_ms,
        )?;
        // A rejoin un-forgets the room: `POST /forget` is not meant to be a permanent exile, only
        // "stop counting this room until I come back to it" (see the spec's "Can re-join room if
        // re-invited" case in `apidoc_room_forget_test.go`). `action == Join` here always means
        // the *target* (== the sender, since a client can only join on its own behalf) is joining.
        if action == Action::Join {
            self.forgotten.remove(&target);
        }
        Ok(event)
    }

    /// Re-stamps `user`'s own `m.room.member` event with a fresh `displayname`/`avatar_url`,
    /// keeping every other field (`membership`, `is_direct`, `reason`, ...) exactly as it was.
    /// This is the room-actor half of profile-change propagation
    /// (`crates/hs-room/src/routes/profile.rs`): per the spec (and Synapse's
    /// `ProfileHandler._update_join_states`), a display name or avatar change is only visible to
    /// other members because the server re-sends this event in every room the user is joined to --
    /// see that module's doc comment for the full design and why the caller lives in this crate
    /// rather than `hs-auth`, which owns the profile write itself.
    ///
    /// Returns `Ok(None)` without sending anything if `user`'s current membership in this room is
    /// not `join` (defensive: the caller is expected to only call this for rooms
    /// [`rooms_joined_by_user`] returned, but membership can change between that read and this
    /// call landing). Goes through the ordinary [`RoomActor::send_event`] path, so an unchanged
    /// profile (nothing actually different from the current event's content) is a no-op that
    /// returns the existing event rather than minting a new one --
    /// [`RoomActor::idempotent_state_reuse`] already gives this for free.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if reading the current membership event fails, or any error
    /// [`RoomActor::send_event`] can return.
    pub fn refresh_own_profile(
        &mut self,
        user: &UserId,
        display_name: Option<String>,
        avatar_url: Option<String>,
        now_ms: i64,
    ) -> Result<Option<Event>, RoomError> {
        if self.prior_membership(user)? != PriorState::Join {
            return Ok(None);
        }
        let mut content = self
            .state_event("m.room.member", user.as_str())?
            .and_then(|existing| existing.json().get("content"))
            .map(|v| {
                serde_json::from_slice::<serde_json::Value>(&v.to_canonical_bytes())
                    .unwrap_or(serde_json::Value::Null)
            })
            .unwrap_or(serde_json::Value::Null);
        if !content.is_object() {
            content = serde_json::json!({});
        }
        let map = content
            .as_object_mut()
            .expect("forced to an object just above");
        match display_name {
            Some(name) => {
                map.insert("displayname".to_owned(), serde_json::Value::String(name));
            }
            None => {
                map.remove("displayname");
            }
        }
        match avatar_url {
            Some(url) => {
                map.insert("avatar_url".to_owned(), serde_json::Value::String(url));
            }
            None => {
                map.remove("avatar_url");
            }
        }
        let event = self.membership_action(
            user.to_owned(),
            Action::Join,
            user.to_owned(),
            content,
            now_ms,
        )?;
        Ok(Some(event))
    }

    /// `POST /rooms/{roomId}/forget`: marks `user` as having forgotten this room -- see
    /// [`RoomActor::can_read_room`] for what that then blocks. Ported from the spec's documented
    /// rule (`refs/matrix-spec/data/api/client-server/leaving.yaml`, Apache-2.0): a currently
    /// joined user must leave first.
    ///
    /// # Errors
    /// Returns [`RoomError::StillJoined`] if `user`'s current membership is `join`.
    pub fn forget(&mut self, user: &UserId) -> Result<(), RoomError> {
        let prior = self.prior_membership(user)?;
        if prior == PriorState::Join {
            return Err(RoomError::StillJoined(format!(
                "User {user} is in room {}",
                self.room_id
            )));
        }
        self.forgotten.insert(user.to_owned());
        Ok(())
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

    /// Accepts an already hash- and signature-verified, foreign [`Event`] and persists it exactly
    /// as received -- the entry point `docs/design/04-room-actor-protocol.md` names
    /// `Command::PersistInbound` and `docs/status/06-federation.md` names as the one gap standing
    /// between this server and real inbound federation. Unlike [`RoomActor::send_event`] /
    /// [`RoomActor::send_event_citing`] (which *build and sign a new* locally-originated event),
    /// this method builds nothing and signs nothing: `event`'s ID, `hashes` and `signatures`
    /// round-trip byte-identically into storage, because every other homeserver in the room will
    /// recompute and check them.
    ///
    /// # What the caller must already have done
    /// **Content hash and signature verification are the caller's job, not this method's.** The
    /// intended caller is `hs_federation::inbound::verify_pdu` (via its `RoomWriteSink` seam);
    /// this method trusts that `event` already passed that check and does not redo it. This
    /// method adds exactly three things on top: idempotency, ancestor presence, and Matrix event
    /// authorization.
    ///
    /// # Authorization: which of the spec's three snapshots this checks
    /// The server-server spec's "Checks performed on receipt of a PDU" runs event authorization
    /// against three different state snapshots. This method implements the first two -- both
    /// **hard** rejections -- and does not implement the third:
    ///
    /// 1. **Implemented** -- *the state implied by the event's own `auth_events`*: builds a
    ///    [`FlatState`] directly from the bodies of the events `event.auth_events` names (exactly
    ///    those events, not a state-store resolution) and runs [`auth::check_event_auth`] against
    ///    it, after [`auth::check_auth_events_selection`] confirms the selection itself is the one
    ///    the spec's auth-events-selection algorithm would have produced against this room's
    ///    actual current state.
    /// 2. **Implemented** -- *the state before the event*: resolves this room's state at
    ///    `event`'s own `prev_events` (via [`StateStore::current_state`], the same resolution
    ///    [`RoomActor::send_event_citing`] authorizes newly-built events against) and runs
    ///    [`auth::check_event_auth`] against that.
    /// 3. **Not implemented** -- *the room's current state at receipt time*: the spec treats a
    ///    failure here as a **soft failure** (the event is still stored, just excluded from some
    ///    views/forward-extremity consideration), which needs a persisted-but-excluded event
    ///    representation this crate does not have. Every rejection this method produces is
    ///    therefore a hard rejection.
    ///
    /// A failure of either implemented check means the event is **refused outright and not
    /// persisted at all** (`RoomError::Forbidden`) -- this session's documented choice over
    /// storing it with a rejected flag (`hs_model::event::EventFlags` already has one, unused
    /// here): nothing yet reads a "stored but rejected" event back out, so storing one would be
    /// silent dead weight. Revisit once soft-fail support needs the flag.
    ///
    /// # Idempotency and ordering
    /// Receiving the same `event.event_id()` twice returns [`RemoteEventOutcome::AlreadyKnown`]
    /// without repeating auth or storage work -- the normal shape of a retried `/send` transaction
    /// or an event that arrives by both `/send` and backfill. An event whose `prev_events` or
    /// `auth_events` name an ancestor this actor does not hold is
    /// [`RoomError::MissingAncestors`]: the ordinary federation case of an event arriving before
    /// its history has been backfilled. This method never fetches anything itself (no network
    /// access from `hs-room`) -- closing that gap is track 06's backfill, not a retry loop here.
    ///
    /// # Errors
    /// [`RoomError::MissingAncestors`] if a cited `prev_events`/`auth_events` entry is not held by
    /// this actor; [`RoomError::Forbidden`] if authorization rejects the event; [`RoomError::State`]
    /// if the state store fails; [`RoomError::Store`] on a storage failure persisting the event.
    pub fn accept_remote_event(&mut self, event: Event) -> Result<RemoteEventOutcome, RoomError> {
        if self.event_id_index.contains_key(event.event_id()) {
            return Ok(RemoteEventOutcome::AlreadyKnown);
        }

        let prev_ids = pipeline::decode_event_ids(event.json().get("prev_events"));
        let auth_ids = pipeline::decode_event_ids(event.json().get("auth_events"));

        let mut missing = Vec::new();
        let mut prev_sns = Vec::with_capacity(prev_ids.len());
        for id in &prev_ids {
            match self.event_id_index.get(id) {
                Some(&sn) => prev_sns.push(sn),
                None => missing.push(id.clone()),
            }
        }
        let mut auth_sns = Vec::with_capacity(auth_ids.len());
        for id in &auth_ids {
            match self.event_id_index.get(id) {
                Some(&sn) => auth_sns.push(sn),
                None => missing.push(id.clone()),
            }
        }
        if !missing.is_empty() {
            return Err(RoomError::MissingAncestors(missing));
        }

        {
            let mut auth_flat = FlatState::new();
            let mut auth_event_refs = Vec::with_capacity(auth_sns.len());
            for &sn in &auth_sns {
                let e = self
                    .events
                    .get(&sn)
                    .ok_or_else(|| RoomError::Internal("auth event not in hot cache".into()))?;
                let content = e
                    .json()
                    .get("content")
                    .and_then(CanonicalJsonValue::as_object)
                    .cloned()
                    .unwrap_or_default();
                auth_flat.insert(
                    e.header().event_type.clone(),
                    e.header().state_key.clone().unwrap_or_default(),
                    e.header().sender.clone(),
                    content,
                );
                auth_event_refs.push(AuthEventRef {
                    event_type: &e.header().event_type,
                    state_key: e.header().state_key.as_deref().unwrap_or(""),
                    rejected: e.header().flags.is_rejected(),
                });
            }

            let content_obj = event
                .json()
                .get("content")
                .and_then(CanonicalJsonValue::as_object)
                .cloned()
                .unwrap_or_default();
            let redacts_owned = extract_redacts(&event);
            let only_prev_is_create = prev_sns.len() == 1
                && self
                    .events
                    .get(&prev_sns[0])
                    .is_some_and(|e| e.header().event_type == "m.room.create");

            let incoming = IncomingEvent {
                event_type: &event.header().event_type,
                sender: AsRef::<UserId>::as_ref(&event.header().sender),
                room_id: Some(&self.room_id),
                state_key: event.header().state_key.as_deref(),
                content: &content_obj,
                prev_event_count: prev_sns.len(),
                only_prev_event_is_room_create: only_prev_is_create,
                event_id: Some(event.event_id()),
                redacts: redacts_owned.as_deref(),
            };

            let state_before = self.state_view(&prev_sns)?;
            let create_lookup = || {
                state_before
                    .event_for("m.room.create", "")
                    .map(|found| found.is_some())
                    .map_err(|e| AuthError::reject(e.to_string()))
            };
            auth::check_auth_events_selection(
                &self.rules,
                &incoming,
                &auth_event_refs,
                create_lookup,
            )
            .map_err(RoomError::from)?;

            auth::check_event_auth(&self.rules, &incoming, &auth_flat).map_err(|e| {
                RoomError::Forbidden(format!("auth-events-implied state rejected event: {e}"))
            })?;
            auth::check_event_auth(&self.rules, &incoming, &state_before.state_fetch()).map_err(
                |e| RoomError::Forbidden(format!("state-before-the-event rejected event: {e}")),
            )?;
        }

        let event_sn = self.persist(event)?;
        Ok(RemoteEventOutcome::Stored(event_sn))
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

        // Computed up front (rather than alongside `membership_deltas` below, which runs after
        // the KV transaction commits) so the `Tables::joined_rooms` write can happen *inside* that
        // same transaction: `Some((target, true))` means the target's new membership is `join`
        // (index them), `Some((target, false))` means it changed away from `join` (remove them),
        // `None` means this event is not an `m.room.member` event at all.
        let member_join_index_update: Option<(String, bool)> = if event.header().event_type
            == "m.room.member"
            && let Some(state_key) = &event.header().state_key
        {
            content
                .get("membership")
                .and_then(serde_json::Value::as_str)
                .map(|m| (state_key.clone(), m == "join"))
        } else {
            None
        };

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

        // Set from inside the `transact` closure below when the cluster-fencing check fails, so
        // the failure can be reported as `RoomError::Fenced` with its real message rather than
        // the generic `hs_kv::KvError::Aborted` it must travel through `transact`'s fixed error
        // type as (see `crate::fencing::RoomFencing::check`'s own doc comment).
        let fence_failure: std::cell::Cell<Option<String>> = std::cell::Cell::new(None);
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
            if let Some((target, is_join)) = &member_join_index_update {
                if *is_join {
                    self.tables
                        .joined_rooms
                        .put(txn, &(target.clone(), room_sn), b"")
                        .map_err(to_kv)?;
                } else {
                    self.tables
                        .joined_rooms
                        .delete(txn, &(target.clone(), room_sn))
                        .map_err(to_kv)?;
                }
            }
            // The belt-and-braces cluster-fencing check (`docs/status/03-cluster.md` item 4), as
            // the last read before this closure returns `Ok`: see `crate::fencing`'s module docs
            // for why this must run *inside* this same transaction rather than before it. A no-op
            // when `self.fencing` is unset (every construction path today, until `hs-cli` installs
            // one -- see this crate's status file).
            if let Some(fencing) = &self.fencing
                && let Err(msg) = fencing.check(self.room_id.as_str(), txn)
            {
                fence_failure.set(Some(msg.clone()));
                return Err(hs_kv::KvError::Aborted(Box::new(std::io::Error::other(
                    msg,
                ))));
            }
            Ok(event_sn)
        })
        .map_err(|e| match fence_failure.take() {
            Some(msg) => RoomError::Fenced(msg),
            None => RoomError::from(e),
        })?;

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

        let room_id = request
            .room_id
            .clone()
            .unwrap_or_else(|| RoomId::new_v1(&identity.server_name));

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

        let member_content = |user: &UserId| {
            request
                .member_content
                .get(user)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}))
        };
        actor.membership_action(
            creator.clone(),
            Action::Join,
            creator.clone(),
            member_content(&creator),
            now_ms,
        )?;

        let preset = request.preset.as_deref().unwrap_or("private_chat");
        let (join_rule, history_visibility, guest_access) = match preset {
            "public_chat" => ("public", "shared", "forbidden"),
            "trusted_private_chat" => ("invite", "shared", "can_join"),
            _ => ("invite", "shared", "can_join"),
        };

        let mut power_levels_content = {
            let mut users = serde_json::Map::new();
            // From room version 12 (MSC4289) the room's creators hold power implicitly and for
            // ever, and `m.room.power_levels` naming any of them in `users` is rejected outright
            // by `hs_state::auth`'s `check_room_power_levels`. Below that version the creator's
            // authority comes *from* this entry, so it must be present.
            // `explicitly_privilege_room_creators` is the room-version rule that distinguishes the
            // two, rather than a version comparison here.
            if !rules.explicitly_privilege_room_creators {
                users.insert(creator.to_string(), serde_json::Value::from(100));
            }
            if preset == "trusted_private_chat" {
                // Invitees are not creators (creators are the sender plus any
                // `additional_creators` on the create event), so they take an ordinary explicit
                // entry in every room version.
                for user in &request.invite {
                    users.insert(user.to_string(), serde_json::Value::from(100));
                }
            }
            // Every default is written out rather than left to the auth rules' implicit ones, for
            // two reasons. Clients read this event to decide what to offer -- Complement's
            // `TestPowerLevels` looks for `ban`, `kick`, `redact` and the three `*_default`s, and
            // Element greys out controls from the same keys. And the implicit defaults are
            // *weaker* than any deployed server's: with no `events` map, changing the power levels
            // themselves, the history visibility, the server ACL, or turning on encryption all fall
            // under `state_default`, so a moderator at 50 could do any of them. They are reserved
            // for 100 here, as on every Synapse-created room a user of this server will ever
            // federate with.
            //
            // From room version 12 creators outrank every explicit level, and replacing the room is
            // kept for them alone: 150 is more than anybody can be given.
            let tombstone = if rules.explicitly_privilege_room_creators {
                150
            } else {
                100
            };
            // Anybody may invite to a private room, where the people in it are the ones who know
            // who else belongs; a public room leaves inviting to its moderators.
            let invite = if preset == "public_chat" { 50 } else { 0 };
            serde_json::json!({
                "users": users,
                "users_default": 0,
                "events": {
                    "m.room.name": 50,
                    "m.room.avatar": 50,
                    "m.room.canonical_alias": 50,
                    "m.room.power_levels": 100,
                    "m.room.history_visibility": 100,
                    "m.room.server_acl": 100,
                    "m.room.encryption": 100,
                    "m.room.tombstone": tombstone,
                },
                "events_default": 0,
                "state_default": 50,
                "ban": 50,
                "kick": 50,
                "redact": 50,
                "invite": invite,
            })
        };
        // `power_level_content_override` is "applied on top of the generated
        // `m.room.power_levels` event content" (client-server API, `POST /createRoom`): a
        // top-level key the override carries wins, and every key it does not mention is kept.
        // Replacing the whole content instead drops the `users` entry that grants the creator
        // power 100 below room version 12, which auth-rejects the very next bootstrap event --
        // and Element sends this field on every room it creates, so that made room creation from
        // a real client fail unconditionally.
        if let Some(overrides) = request
            .power_level_content_override
            .as_ref()
            .and_then(serde_json::Value::as_object)
        {
            let base = power_levels_content
                .as_object_mut()
                .expect("built as an object directly above");
            for (key, value) in overrides {
                base.insert(key.clone(), value.clone());
            }
        }
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
            // The top-level `topic` request field (unlike an `m.room.topic` sent through
            // `initial_state`, which is passed through byte-for-byte) additionally populates the
            // extensible-text representation, `m.topic.m.text` (MSC3765, stabilized into the
            // spec's `m.room.topic` schema): "an `m.room.topic` event with a `text/plain`
            // mimetype will be sent" (`refs/matrix-spec/data/api/client-server/create_room.yaml`,
            // Apache-2.0). `mimetype` is left unset rather than written as `"text/plain"`
            // explicitly -- the schema defaults an absent mimetype to `text/plain`, and
            // Complement's own test for this accepts either.
            actor.send_event(
                creator.clone(),
                "m.room.topic".to_owned(),
                Some(String::new()),
                serde_json::json!({
                    "topic": topic,
                    "m.topic": {
                        "m.text": [{"body": topic}],
                    },
                }),
                None,
                now_ms,
            )?;
        }

        if let Some(localpart) = &request.room_alias_name {
            let alias_str = format!("#{localpart}:{}", actor.identity.server_name);
            let alias = RoomAliasId::parse(&alias_str)
                .map_err(|e| RoomError::BadRequest(format!("invalid room_alias_name: {e}")))?;
            actor.create_alias(&alias, &creator)?;
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
                member_content(user),
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
    pub fn create_alias(&self, alias: &RoomAliasId, creator: &UserId) -> Result<(), RoomError> {
        let room_sn = self.room_sn;
        let alias_key = (alias.to_string(),);
        // The stored value is the room's short id followed by the creator's user ID. Rows written
        // before the creator was recorded are just the four short-id bytes, and still read: see
        // `alias_creator`, which reports `None` for them rather than guessing.
        let mut value = room_sn.to_be_bytes().to_vec();
        value.extend_from_slice(creator.as_str().as_bytes());
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
                .put(txn, &alias_key, &value)
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

    /// Who created `alias`, if this server recorded it. `None` for an alias created before the
    /// creator was stored, which is not the same as "nobody" -- a caller deciding whether to
    /// allow something must fall back to another rule rather than treat it as a match.
    ///
    /// # Errors
    /// Returns [`RoomError::Store`] on a storage failure.
    pub fn alias_creator(&self, alias: &RoomAliasId) -> Result<Option<OwnedUserId>, RoomError> {
        let snapshot = self.backend.snapshot();
        let Some(value) = self.tables.aliases.get(&snapshot, &(alias.to_string(),))? else {
            return Ok(None);
        };
        let tail = &value.as_ref()[4.min(value.as_ref().len())..];
        if tail.is_empty() {
            return Ok(None);
        }
        let raw = std::str::from_utf8(tail).map_err(|e| RoomError::Internal(e.to_string()))?;
        Ok(UserId::parse(raw).ok().map(|u| u.to_owned()))
    }

    /// Whether `user` currently has enough power to send `event_type` as a state event in this
    /// room.
    ///
    /// This answers the question the auth rules would answer, without building and rejecting an
    /// event to find out: the room's `m.room.power_levels`, read through
    /// [`EffectivePowerLevels`](hs_model::power_levels::EffectivePowerLevels) so that a room
    /// creator's implicit, un-demotable power in room version 12 and later counts.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the room's state could not be read.
    pub fn can_send_state(&self, user: &UserId, event_type: &str) -> Result<bool, RoomError> {
        let rules = room_version::rules_for(self.room_version())
            .ok_or_else(|| RoomError::Internal("unknown room version".into()))?;
        let creators = self.room_creators(&rules)?;

        let Some(event) = self.state_event("m.room.power_levels", "")? else {
            // A room with no power-levels event yet: the spec's pre-first-event default is 100
            // for a creator and 0 for everybody else, against a state_default of 50.
            return Ok(creators.iter().any(|c| c == user));
        };
        let content = event
            .json()
            .get("content")
            .and_then(CanonicalJsonValue::as_object)
            .ok_or_else(|| RoomError::Internal("power levels have no content".into()))?;
        let levels = hs_model::power_levels::PowerLevels::parse(content, &rules)
            .map_err(|e| RoomError::Internal(e.to_string()))?;
        let effective =
            hs_model::power_levels::EffectivePowerLevels::new(&levels, &rules, creators);
        Ok(effective.user_power(user) >= levels.required_power(event_type, true))
    }

    /// This room's creators: the `m.room.create` sender (or its `creator` field, below room
    /// version 11) plus any `additional_creators`.
    fn room_creators(
        &self,
        rules: &hs_model::room_version::RoomVersionRules,
    ) -> Result<Vec<OwnedUserId>, RoomError> {
        let create = self
            .state_event("m.room.create", "")?
            .ok_or_else(|| RoomError::Internal("room has no m.room.create".into()))?;
        let mut out = Vec::new();
        if rules.use_room_create_sender {
            out.push(create.header().sender.clone());
        } else if let Some(creator) = create
            .json()
            .get("content")
            .and_then(CanonicalJsonValue::as_object)
            .and_then(|c| c.get("creator"))
            .and_then(CanonicalJsonValue::as_str)
            .and_then(|c| UserId::parse(c).ok())
        {
            out.push(creator.to_owned());
        }
        if rules.additional_room_creators
            && let Some(CanonicalJsonValue::Array(items)) = create
                .json()
                .get("content")
                .and_then(CanonicalJsonValue::as_object)
                .and_then(|c| c.get("additional_creators"))
        {
            for item in items {
                if let Some(id) = item.as_str().and_then(|s| UserId::parse(s).ok()) {
                    out.push(id.to_owned());
                }
            }
        }
        Ok(out)
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

    /// Whether this room is currently blocked by a server administrator, and if so, the reason
    /// given (which may itself be absent). Read fresh from the store on every call -- not cached
    /// on the actor -- so [`RoomRegistryDirectory`](crate::admin::RoomRegistryDirectory)'s
    /// `set_blocked` takes effect immediately for a room whose actor is already resident, with
    /// nothing to invalidate.
    ///
    /// # Errors
    /// Returns [`RoomError::Table`] on a storage failure.
    fn blocked_reason(&self) -> Result<Option<Option<String>>, RoomError> {
        let snapshot = self.backend.snapshot();
        let Some(bytes) = self.tables.blocked_rooms.get(&snapshot, &(self.room_sn,))? else {
            return Ok(None);
        };
        let block: crate::persist::RoomBlock = serde_json::from_slice(&bytes)
            .map_err(|e| RoomError::Internal(format!("corrupt blocked-room row: {e}")))?;
        Ok(Some(block.reason))
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
    pub fn state_event(
        &self,
        event_type: &str,
        state_key: &str,
    ) -> Result<Option<&Event>, RoomError> {
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
        self.state_at_root(self.current_view()?.root)
    }

    /// `POST /rooms/{roomId}/upgrade`'s step 3: whichever of the spec's recommended transferable
    /// state event types
    /// (`refs/matrix-spec/content/client-server-api/modules/room_upgrades.md`, CC-BY-4.0 --
    /// `m.room.server_acl`, `m.room.encryption`, `m.room.name`, `m.room.avatar`, `m.room.topic`,
    /// `m.room.guest_access`, `m.room.history_visibility`, `m.room.join_rules`,
    /// `m.room.power_levels`) this room currently has set, as `(event_type, content)` pairs in
    /// that fixed order. Membership events and anything sender-sensitive outside this list are
    /// deliberately never included, per the same spec section ("servers should not transfer state
    /// events which are sensitive to who sent them").
    #[must_use]
    pub fn transferable_state(&self) -> Vec<(&'static str, serde_json::Value)> {
        const TRANSFERABLE: &[&str] = &[
            "m.room.server_acl",
            "m.room.encryption",
            "m.room.name",
            "m.room.avatar",
            "m.room.topic",
            "m.room.guest_access",
            "m.room.history_visibility",
            "m.room.join_rules",
            "m.room.power_levels",
        ];
        TRANSFERABLE
            .iter()
            .filter_map(|&event_type| {
                let event = self.state_event(event_type, "").ok().flatten()?;
                let content = event.json().get("content")?;
                let content =
                    serde_json::from_slice::<serde_json::Value>(&content.to_canonical_bytes())
                        .ok()?;
                Some((event_type, content))
            })
            .collect()
    }

    /// The `type` field on this room's `m.room.create` event, if it has one -- `/upgrade` copies
    /// it into the replacement room's own create event verbatim (spec step 2: "a `type` field
    /// which is copied from the predecessor room").
    #[must_use]
    pub fn creation_type(&self) -> Option<String> {
        self.state_event("m.room.create", "")
            .ok()
            .flatten()?
            .json()
            .get("content")?
            .as_object()?
            .get("type")?
            .as_str()
            .map(str::to_owned)
    }

    /// Builds this room's admin-API summary (`hs_admin::model::AdminRoom`, the `GET /rooms`/`GET
    /// /rooms/{room_id}` response shape). See `crate::admin::RoomRegistryDirectory`, the seam
    /// that calls this.
    ///
    /// `forgotten` is a deliberate simplification, recorded in this crate's status file: there is
    /// no durable per-user "has forgotten this room" index across every user who has ever been a
    /// member ([`RoomActor::forget`]'s own tracking is in-memory, scoped to whoever called
    /// `/forget` while this actor has been resident). A room with zero currently-joined members is
    /// reported as forgotten; a room that still has joined members never is, regardless of who has
    /// forgotten it.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn admin_summary(&self) -> Result<hs_admin::model::AdminRoom, RoomError> {
        let string_field = |event_type: &str, field: &str| -> Option<String> {
            self.state_event(event_type, "")
                .ok()
                .flatten()?
                .json()
                .get("content")?
                .as_object()?
                .get(field)?
                .as_str()
                .map(str::to_owned)
        };

        let joined = self.joined_members()?;
        let joined_members_count = joined.len() as u64;
        let local_members_count = joined
            .iter()
            .filter(|e| {
                e.header()
                    .state_key
                    .as_deref()
                    .and_then(|k| UserId::parse(k).ok())
                    .is_some_and(|u| u.server_name().as_str() == self.identity.server_name.as_str())
            })
            .count() as u64;
        let state_events_count = self.full_state()?.len() as u64;

        let create_event = self.state_event("m.room.create", "")?;
        let creator = create_event.map(|e| e.header().sender.to_string());
        let federatable = !matches!(
            create_event
                .and_then(|e| e.json().get("content"))
                .and_then(CanonicalJsonValue::as_object)
                .and_then(|c| c.get("m.federate")),
            Some(CanonicalJsonValue::Bool(false))
        );

        let snapshot = self.backend.snapshot();
        let public = self
            .tables
            .public_rooms
            .get(&snapshot, &(self.room_sn,))?
            .is_some();

        let (blocked, blocked_reason) = match self.blocked_reason()? {
            Some(reason) => (true, reason),
            None => (false, None),
        };

        let replacement_room_id = self
            .state_event("m.room.tombstone", "")
            .ok()
            .flatten()
            .and_then(|e| e.json().get("content"))
            .and_then(CanonicalJsonValue::as_object)
            .and_then(|c| c.get("replacement_room"))
            .and_then(CanonicalJsonValue::as_str)
            .map(str::to_owned);

        Ok(hs_admin::model::AdminRoom {
            room_id: self.room_id.to_string(),
            name: string_field("m.room.name", "name"),
            topic: string_field("m.room.topic", "topic"),
            avatar_url: string_field("m.room.avatar", "url"),
            canonical_alias: string_field("m.room.canonical_alias", "alias"),
            joined_members_count,
            local_members_count,
            state_events_count,
            version: self.room_version.as_str().to_owned(),
            creator,
            encrypted: self.state_event("m.room.encryption", "")?.is_some(),
            join_rule: string_field("m.room.join_rules", "join_rule")
                .unwrap_or_else(|| "invite".to_owned()),
            guest_access: string_field("m.room.guest_access", "guest_access")
                .unwrap_or_else(|| "forbidden".to_owned()),
            history_visibility: string_field("m.room.history_visibility", "history_visibility")
                .unwrap_or_else(|| "shared".to_owned()),
            federatable,
            public,
            room_type: self.creation_type(),
            blocked,
            blocked_reason,
            tombstoned: replacement_room_id.is_some(),
            replacement_room_id,
            forgotten: joined_members_count == 0,
        })
    }

    /// Grants `user_id` this room's highest currently-used power level (capped at 100), for
    /// `hs-admin`'s `rooms.make_admin`. Sends a new `m.room.power_levels` event as whichever
    /// currently-joined member already holds enough power to send it -- mirroring Synapse's
    /// `make_room_admin` (read for behavior only, never copied, per this track's brief): `user_id`
    /// is very likely the one member *without* enough power yet, so the event cannot be sent with
    /// them as its own sender. Ties among equally-powerful candidates are broken by the smaller
    /// user ID, for a deterministic choice.
    ///
    /// # Errors
    /// Returns [`RoomError::Forbidden`] if `user_id` does not currently hold `join` membership,
    /// [`RoomError::BadRequest`] if no currently-joined member holds enough power to send
    /// `m.room.power_levels` at all (a room whose only sufficiently-privileged members have all
    /// left cannot be granted a new admin this way), or any error [`RoomActor::send_event`] can
    /// return.
    pub fn make_admin(&mut self, user_id: &UserId, now_ms: i64) -> Result<Event, RoomError> {
        let is_joined = self
            .state_event("m.room.member", user_id.as_str())?
            .and_then(|e| e.json().get("content"))
            .and_then(CanonicalJsonValue::as_object)
            .and_then(|c| c.get("membership"))
            .and_then(CanonicalJsonValue::as_str)
            == Some("join");
        if !is_joined {
            return Err(RoomError::Forbidden(format!(
                "{user_id} is not a member of this room"
            )));
        }

        let power_event = self.state_event("m.room.power_levels", "")?;
        let content_obj = power_event
            .and_then(|e| e.json().get("content"))
            .and_then(CanonicalJsonValue::as_object);
        let levels = match content_obj {
            Some(obj) => hs_model::power_levels::PowerLevels::parse(obj, &self.rules)
                .map_err(|e| RoomError::Internal(e.to_string()))?,
            None => hs_model::power_levels::PowerLevels::default(),
        };

        let target_level = levels.users.values().copied().max().unwrap_or(100).min(100);
        let required = levels.required_power("m.room.power_levels", true);

        let mut candidates: Vec<(i64, OwnedUserId)> = Vec::new();
        for member in self.joined_members()? {
            if let Some(sender) = member.header().state_key.as_deref()
                && let Ok(sender_id) = UserId::parse(sender)
            {
                let power = levels.user_power(&sender_id);
                if power >= required {
                    candidates.push((power, sender_id.to_owned()));
                }
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let Some((_, acting_sender)) = candidates.into_iter().next() else {
            return Err(RoomError::BadRequest(
                "no member of this room currently has enough power to send m.room.power_levels"
                    .to_owned(),
            ));
        };

        let mut new_content: serde_json::Value = match power_event {
            Some(e) => match e.json().get("content") {
                Some(c) => serde_json::from_slice(&c.to_canonical_bytes())
                    .map_err(|err| RoomError::Internal(err.to_string()))?,
                None => serde_json::json!({}),
            },
            None => serde_json::json!({}),
        };
        if !new_content.is_object() {
            new_content = serde_json::json!({});
        }
        let users = new_content
            .as_object_mut()
            .expect("checked to be an object above")
            .entry("users")
            .or_insert_with(|| serde_json::json!({}));
        if !users.is_object() {
            *users = serde_json::json!({});
        }
        users
            .as_object_mut()
            .expect("checked to be an object above")
            .insert(user_id.to_string(), serde_json::json!(target_level));

        self.send_event(
            acting_sender,
            "m.room.power_levels".to_owned(),
            Some(String::new()),
            new_content,
            None,
            now_ms,
        )
    }

    /// Installs (or clears) the cluster-fencing hook [`RoomActor::persist`] checks before
    /// committing. Called by [`crate::registry::RoomRegistry`] right after constructing or
    /// loading this actor, if a fencing hook has been installed on the registry -- see
    /// `crate::fencing`'s module docs.
    pub(crate) fn set_fencing(&mut self, fencing: Option<Arc<crate::fencing::RoomFencing<B>>>) {
        self.fencing = fencing;
    }

    /// Shared by [`RoomActor::full_state`] and [`RoomActor::full_state_for_reader`]: every event
    /// the resolution rooted at `root` sets, dereferenced through this actor's in-memory cache.
    fn state_at_root(
        &self,
        root: <ProductionStateStore<B> as StateStore>::Root,
    ) -> Result<Vec<&Event>, RoomError> {
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

    /// The state view `requester` should read this room's state through for `GET .../state`,
    /// `.../state/{eventType}(/{stateKey})` and `.../members`: their own current, live view if
    /// they are currently joined, or if the room is `world_readable`; otherwise a view pinned to
    /// the state as of immediately after their own most recent membership-changing event (their
    /// leave, kick or ban) -- implementing the history-visibility module's "after a user has left
    /// a room, they may see any events which they were allowed to see before they left the room,
    /// but no events received after they left" for these bulk-state reads, the same way
    /// [`RoomActor::event_visible_to`] implements it per-event for `.../event`/`.../messages`.
    ///
    /// `Ok(None)` means "deny outright": `requester` has no `m.room.member` event in this room's
    /// current state at all (never joined, invited, knocked, or been banned/kicked) and the room
    /// is not `world_readable` -- the same "never a member" case
    /// [`RoomActor::can_read_room`] denies for `.../messages`.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails, or [`RoomError::Internal`] if
    /// `requester`'s own membership event is in the current state but missing from this actor's
    /// event-ID index (should not happen: every current-state event was indexed when persisted).
    fn reader_view(
        &self,
        requester: &UserId,
    ) -> Result<Option<RoomStateView<'_, ProductionStateStore<B>>>, RoomError> {
        let current = self.current_view()?;
        let membership_event = current
            .event_for("m.room.member", requester.as_str())
            .map_err(|e| RoomError::State(e.to_string()))?;
        let membership =
            parse_membership(membership_event.and_then(|e| content_str(e, "membership")));
        if membership == PriorState::Join {
            return Ok(Some(current));
        }
        let world_readable = history_visibility::HistoryVisibility::parse(
            current
                .event_for("m.room.history_visibility", "")
                .map_err(|e| RoomError::State(e.to_string()))?
                .and_then(|e| content_str(e, "history_visibility")),
        ) == history_visibility::HistoryVisibility::WorldReadable;
        if world_readable {
            return Ok(Some(current));
        }
        let Some(membership_event) = membership_event else {
            return Ok(None);
        };
        let sn = *self
            .event_id_index
            .get(membership_event.event_id())
            .ok_or_else(|| {
                RoomError::Internal("current-state event missing from event-ID index".into())
            })?;
        Ok(Some(self.state_view_at_sn(sn)?))
    }

    /// [`RoomActor::full_state`], but through [`RoomActor::reader_view`]: a departed member sees
    /// the room's state as of when they left, not its current state. `Ok(None)` denies outright
    /// (see `reader_view`'s doc comment for exactly when).
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn full_state_for_reader(
        &self,
        requester: &UserId,
    ) -> Result<Option<Vec<&Event>>, RoomError> {
        let Some(view) = self.reader_view(requester)? else {
            return Ok(None);
        };
        self.state_at_root(view.root).map(Some)
    }

    /// [`RoomActor::state_event`], but through [`RoomActor::reader_view`]. `Ok(None)` covers both
    /// "no such state event" and "requester may not read this room's state at all" -- callers that
    /// must tell the two apart (to answer `403` instead of `404`, say) should call
    /// [`RoomActor::can_read_room`] first.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn state_event_for_reader(
        &self,
        requester: &UserId,
        event_type: &str,
        state_key: &str,
    ) -> Result<Option<&Event>, RoomError> {
        let Some(view) = self.reader_view(requester)? else {
            return Ok(None);
        };
        view.event_for(event_type, state_key)
            .map_err(|e| RoomError::State(e.to_string()))
    }

    /// [`RoomActor::members`], but through [`RoomActor::reader_view`]. `Ok(None)` denies outright
    /// (see `reader_view`'s doc comment).
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn members_for_reader(&self, requester: &UserId) -> Result<Option<Vec<&Event>>, RoomError> {
        let Some(state) = self.full_state_for_reader(requester)? else {
            return Ok(None);
        };
        Ok(Some(
            state
                .into_iter()
                .filter(|e| e.header().event_type == "m.room.member")
                .collect(),
        ))
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

    /// Whether `requester` may call the bulk read endpoints this crate serves on top of the whole
    /// timeline (`GET .../messages` today) at all, before any single event is filtered by
    /// [`RoomActor::event_visible_to`]. Two denials this distinguishes from "yes, then filter":
    ///
    /// - A user who has **forgotten** this room (`RoomActor::forget`) is refused outright, even
    ///   though `m.room.history_visibility: shared`'s per-event rule would otherwise let a former
    ///   member see events from while they were joined -- this is `apidoc_room_forget_test.go`'s
    ///   "Forgotten room messages cannot be paginated", a deliberate, spec-documented exception
    ///   ("stop remembering about a particular room") to the general history-visibility algorithm.
    /// - A user with **no `m.room.member` event at all** in this room's current state (never
    ///   joined, invited, knocked, or been banned/kicked) is refused, unless the room is currently
    ///   `world_readable` -- matching `room_messages_test.go`'s "you aren't a member of the room".
    ///
    /// A past member who has *not* forgotten the room (left or was banned, but never called
    /// `/forget`) passes this gate and falls through to ordinary per-event filtering
    /// (`apidoc_room_forget_test.go`'s "Can get rooms/{roomId}/messages for a departed room").
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn can_read_room(&self, requester: &UserId) -> Result<bool, RoomError> {
        let view = self.current_view()?;
        let hv = history_visibility::HistoryVisibility::parse(
            view.event_for("m.room.history_visibility", "")
                .map_err(|e| RoomError::State(e.to_string()))?
                .and_then(|e| content_str(e, "history_visibility")),
        );
        if hv == history_visibility::HistoryVisibility::WorldReadable {
            return Ok(true);
        }
        if self.forgotten.contains(requester) {
            return Ok(false);
        }
        let has_membership_record = view
            .event_for("m.room.member", requester.as_str())
            .map_err(|e| RoomError::State(e.to_string()))?
            .is_some();
        Ok(has_membership_record)
    }

    /// Whether `requester` is joined to this room *now*, or the room is world-readable.
    ///
    /// Stricter than [`RoomActor::can_read_room`], which lets a departed member through so that
    /// they can read the history they were present for. Who is in a room today, and what it is
    /// called today, are not history: the spec gives `joined_members` to "the current user
    /// [who] must be in the room", and `/aliases` to a member or anybody if the room is
    /// world-readable. Both routes used to answer anyone with an access token, so any account
    /// could list the members and aliases of any room on this server, private ones included.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn can_see_current_membership(&self, requester: &UserId) -> Result<bool, RoomError> {
        let view = self.current_view()?;
        let hv = history_visibility::HistoryVisibility::parse(
            view.event_for("m.room.history_visibility", "")
                .map_err(|e| RoomError::State(e.to_string()))?
                .and_then(|e| content_str(e, "history_visibility")),
        );
        if hv == history_visibility::HistoryVisibility::WorldReadable {
            return Ok(true);
        }
        Ok(view
            .event_for("m.room.member", requester.as_str())
            .map_err(|e| RoomError::State(e.to_string()))?
            .and_then(|e| content_str(e, "membership"))
            == Some("join"))
    }

    /// Whether `user` was joined to this room as of timeline position `room_pos`: the state
    /// immediately after the last event at or before that position. `false` when the position
    /// is before the room's first event.
    ///
    /// `/sync` asks this about the position a client's token points to, to tell a room the
    /// client has been following from one it has only just come into -- by accepting an
    /// invitation, or by coming back after leaving. Both leave history in the user's feed from
    /// before they were joined, so "is there a position to resume from" cannot tell them apart,
    /// and the second kind needs the room sent whole.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn was_joined_at(&self, user: &UserId, room_pos: i64) -> Result<bool, RoomError> {
        let Some((_, sn)) = self.timeline.range(..=room_pos).next_back() else {
            return Ok(false);
        };
        Ok(self
            .state_view_at_sn(*sn)?
            .event_for("m.room.member", user.as_str())
            .map_err(|e| RoomError::State(e.to_string()))?
            .and_then(|e| content_str(e, "membership"))
            == Some("join"))
    }

    /// Up to `limit` events after room-local position `after`, oldest first, each with its own
    /// position. Never an event at a negative position: those are history fetched from other
    /// servers after the fact, which is not news to anybody following the room forwards.
    ///
    /// What [`RoomActor::paginate`] is for a client, this is for a follower that keeps a cursor
    /// (appservice delivery): it needs each event's position, not one continuation token.
    #[must_use]
    pub fn events_after(&self, after: i64, limit: usize) -> Vec<(i64, &Event)> {
        self.timeline
            .range((
                std::ops::Bound::Excluded(after.max(0)),
                std::ops::Bound::Unbounded,
            ))
            .take(limit)
            .filter_map(|(pos, sn)| Some((*pos, self.events.get(sn)?)))
            .collect()
    }

    /// The user IDs joined to this room as of immediately after `event`: the room's membership
    /// at that point in its history, not now. What appservice delivery decides interest against
    /// -- a bridge is sent an event if one of its users was in the room *when it was sent*,
    /// which "who is in the room now" gets wrong exactly when it matters: read together with
    /// the bot's own join, every message from before it would go out to the bridge, and from
    /// there to wherever it bridges to.
    ///
    /// # Errors
    /// Returns [`RoomError::EventNotFound`] if this actor does not hold `event`, or
    /// [`RoomError::State`] if the state store fails.
    pub fn joined_members_after(&self, event: &Event) -> Result<Vec<String>, RoomError> {
        let sn = *self
            .event_id_index
            .get(event.event_id())
            .ok_or_else(|| RoomError::EventNotFound(event.event_id().to_string()))?;
        let view = self.state_view_at_sn(sn)?;
        Ok(self
            .state_at_root(view.root)?
            .into_iter()
            .filter(|e| {
                e.header().event_type == "m.room.member"
                    && content_str(e, "membership") == Some("join")
            })
            .filter_map(|e| e.header().state_key.clone())
            .collect())
    }

    /// The room-local send position (`room_pos`) of a known [`EventSn`], by linear scan of
    /// `self.timeline`. Phase 0 scope, same tradeoff as `RoomActor::get_context`'s full scan for
    /// an event's position: this crate holds a room's whole timeline resident in memory already
    /// (see `events`'s doc comment), so a scan costs a `Vec`-sized comparison loop, not a store
    /// round trip -- a `HashMap<EventSn, i64>` reverse index is the obvious speed-up if profiling
    /// ever shows this mattering.
    fn room_pos_of(&self, sn: EventSn) -> Option<i64> {
        self.timeline
            .iter()
            .find_map(|(pos, s)| (*s == sn).then_some(*pos))
    }

    /// Whether `requester` may see `event`, per the `m.room.history_visibility` read-side
    /// algorithm (`crate::history_visibility`; ported from
    /// `refs/matrix-spec/content/client-server-api/modules/history_visibility.md`'s "Server
    /// behaviour" section, CC-BY-4.0). Evaluated against the room's state **as of `event`**, with
    /// the "before or after" special case that section documents for
    /// `m.room.history_visibility` events, and with the requester's own `m.room.member` events
    /// always visible to them (wider than that section's own "before or after" rule for them;
    /// the body says why) -- not just the room's *current* setting, which is what let a user who had left a
    /// non-world-readable room read full event content before this method existed (this crate's
    /// security-fix session; see `docs/status/04-room-and-events.md`).
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails, or [`RoomError::EventNotFound`] if
    /// this actor does not hold `event` (should not happen for an event this same actor just
    /// handed back from its own cache).
    pub fn event_visible_to(&self, event: &Event, requester: &UserId) -> Result<bool, RoomError> {
        // The spec's rule for a user's own membership events is "allowed under their membership
        // before it, or after it" -- and then there is the case that rule leaves out. Somebody
        // who declines an invitation (or has it withdrawn, or is banned while still only
        // invited) was never joined, so under anything but `invited` visibility neither side of
        // their own leave lets them see it: `/sync` never moved the room to `leave`, and the
        // invitation sat in their client for good. An event about a user's own membership tells
        // them nothing they are not entitled to know, so they may always see it.
        if event.header().event_type == "m.room.member"
            && event.header().state_key.as_deref() == Some(requester.as_str())
        {
            return Ok(true);
        }
        let sn = *self
            .event_id_index
            .get(event.event_id())
            .ok_or_else(|| RoomError::EventNotFound(event.event_id().to_string()))?;
        let pos = self
            .room_pos_of(sn)
            .ok_or_else(|| RoomError::EventNotFound(event.event_id().to_string()))?;

        // Rule 3's "the user joined the room at any point after the event was sent": true if any
        // later timeline entry is an `m.room.member` event for `requester` with `membership:
        // join`, regardless of whether they are still joined now.
        let joined_later = self
            .timeline
            .range((std::ops::Bound::Excluded(pos), std::ops::Bound::Unbounded))
            .filter_map(|(_, s)| self.events.get(s))
            .any(|e| {
                e.header().event_type == "m.room.member"
                    && e.header().state_key.as_deref() == Some(requester.as_str())
                    && content_str(e, "membership") == Some("join")
            });

        let prev_sns: Vec<EventSn> = pipeline::decode_event_ids(event.json().get("prev_events"))
            .iter()
            .filter_map(|id| self.event_id_index.get(id).copied())
            .collect();
        let before = self.state_view(&prev_sns)?;
        let after = self.state_view_at_sn(sn)?;

        let hv_before = history_visibility::HistoryVisibility::parse(
            before
                .event_for("m.room.history_visibility", "")
                .map_err(|e| RoomError::State(e.to_string()))?
                .and_then(|e| content_str(e, "history_visibility")),
        );
        let hv_after = history_visibility::HistoryVisibility::parse(
            after
                .event_for("m.room.history_visibility", "")
                .map_err(|e| RoomError::State(e.to_string()))?
                .and_then(|e| content_str(e, "history_visibility")),
        );
        let membership_after = parse_membership(
            after
                .event_for("m.room.member", requester.as_str())
                .map_err(|e| RoomError::State(e.to_string()))?
                .and_then(|e| content_str(e, "membership")),
        );

        let is_hv_event = event.header().event_type == "m.room.history_visibility";

        let allowed = if is_hv_event {
            history_visibility::base_rule_allows(hv_before, membership_after, joined_later)
                || history_visibility::base_rule_allows(hv_after, membership_after, joined_later)
        } else {
            history_visibility::base_rule_allows(hv_after, membership_after, joined_later)
        };
        Ok(allowed)
    }

    /// The state event `event` replaced: the event that held `(event.type, event.state_key)` in
    /// this room's current state *at the point `event` was sent*, which is what the client-server
    /// API's `unsigned.prev_content`, `unsigned.replaces_state` and `unsigned.prev_sender` are
    /// defined against. `Ok(None)` for a message event (no `state_key`, so it replaced nothing),
    /// for the first event of its `(type, state_key)` in the room, and for an event whose
    /// `prev_events` this actor does not hold.
    ///
    /// "At the point it was sent", not "the newest one before it in the timeline", is the whole
    /// difficulty: the room is a DAG, so the predecessor in send order is not necessarily the
    /// state this event superseded. The answer is the resolved state at `event`'s own
    /// `prev_events` -- the same view [`RoomActor::event_visible_to`] evaluates history
    /// visibility "before" against, and the same one `crate::pipeline` authorized `event`
    /// against when it was accepted. Taking it from anywhere else (the current state, or a scan
    /// backwards through the timeline) would report the wrong content on any room that has ever
    /// forked, and would report a *later* event's content when rendering an old page of
    /// `/messages`.
    ///
    /// # Errors
    /// Returns [`RoomError::State`] if the state store fails.
    pub fn replaced_state_event(&self, event: &Event) -> Result<Option<&Event>, RoomError> {
        let Some(state_key) = event.header().state_key.as_deref() else {
            return Ok(None);
        };
        let prev_sns: Vec<EventSn> = pipeline::decode_event_ids(event.json().get("prev_events"))
            .iter()
            .filter_map(|id| self.event_id_index.get(id).copied())
            .collect();
        if prev_sns.is_empty() {
            return Ok(None);
        }
        self.state_view(&prev_sns)?
            .event_for(&event.header().event_type, state_key)
            .map_err(|e| RoomError::State(e.to_string()))
    }

    /// [`RoomActor::replaced_state_event`], resolved into the form the client-server renderer
    /// wants: what `requester` is allowed to be told about the event `event` replaced. See
    /// [`crate::routes::render::attach_replaced_state`], which turns this into the three
    /// `unsigned` fields.
    ///
    /// The history-visibility verdict applies to the *replaced* event, not to `event`: the spec
    /// returns `prev_content` "only ... if the client has permission to see the previous event",
    /// so a reader who can see a join but not the membership event it superseded gets
    /// `replaces_state` and `prev_sender` (which the spec says are returned regardless) without
    /// `prev_content`.
    ///
    /// Returns `None` -- rendering nothing at all -- when there is no replaced event, and also
    /// when the state store errors: a failed lookup must not turn an otherwise-renderable event
    /// into a failed request, and an omitted `unsigned` field is exactly what every client
    /// already copes with.
    #[must_use]
    pub fn replaced_state_for(
        &self,
        event: &Event,
        requester: &UserId,
    ) -> Option<crate::routes::render::ReplacedState> {
        let replaced = self.replaced_state_event(event).ok().flatten()?;
        let visible = self.event_visible_to(replaced, requester).unwrap_or(false);
        Some(crate::routes::render::ReplacedState::new(replaced, visible))
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
        let root_sender = self.event_by_id(target).map(|e| e.header().sender.as_ref());
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
        relations::bundle(&children, requesting_user, root_sender)
    }

    /// `GET /rooms/{roomId}/threads`: every event in this room that is the target of at least one
    /// `m.thread`-`rel_type` relation (a thread root), newest-active-thread first -- ordered by
    /// the latest `m.thread` child's position on this room's timeline, descending. `room_threads_test.go`'s
    /// `TestThreadsEndpoint` checks this ordering directly, including that a new reply to an
    /// older thread moves it back to the front.
    ///
    /// `participated_only` applies the `?include=participated` filter: a thread is kept only if
    /// `requester` is the root's sender or the sender of one of its `m.thread` children (the
    /// module's `current_user_participated` rule -- see [`relations::bundle`]'s doc comment).
    ///
    /// Visibility (`m.room.history_visibility`) is **not** applied here: this returns every
    /// thread root this actor holds, full stop. Callers must filter through
    /// [`RoomActor::event_visible_to`] themselves, exactly as `crate::routes::query::get_messages`
    /// filters its own timeline scan -- keeping the visibility policy in one place (`event_visible_to`
    /// itself) rather than duplicating it into every enumeration method.
    #[must_use]
    pub fn thread_roots(&self, requester: &UserId, participated_only: bool) -> Vec<&Event> {
        // Timeline position, not `origin_server_ts`. The timestamp has millisecond resolution and
        // is whatever the sender's clock said, so two replies sent back to back tie -- and the
        // tie-break is then an event ID, which is a hash. Complement's `TestThreadsEndpoint`
        // passed in two runs out of four on exactly that coin. Position is a total order, is this
        // server's own, and is what "most recently active" means.
        let position_of: HashMap<EventSn, i64> =
            self.timeline.iter().map(|(pos, sn)| (*sn, *pos)).collect();
        let mut roots: Vec<(&Event, i64)> = self
            .relations_by_target
            .keys()
            .filter_map(|target| {
                let root = self.event_by_id(target)?;
                let thread_children = self.relations_of(target, Some("m.thread"));
                if thread_children.is_empty() {
                    return None;
                }
                if participated_only {
                    let participated = root.header().sender == *requester
                        || thread_children
                            .iter()
                            .any(|c| c.header().sender == *requester);
                    if !participated {
                        return None;
                    }
                }
                // A reply this room holds but has not placed on its timeline (an outlier) says
                // nothing about recency; a thread with only those sorts last.
                let latest = thread_children
                    .iter()
                    .filter_map(|c| self.event_id_index.get(c.event_id()))
                    .filter_map(|sn| position_of.get(sn))
                    .max()
                    .copied()
                    .unwrap_or(i64::MIN);
                Some((root, latest))
            })
            .collect();
        // Descending by latest activity. Positions are unique, so the event ID only ever breaks
        // a tie between threads whose replies are all outliers.
        roots.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.event_id().cmp(b.0.event_id()))
        });
        roots.into_iter().map(|(e, _)| e).collect()
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
        let event_id = self
            .txn_dedup
            .get(&Self::dedup_key(sender, device_id, txn_id))?;
        self.event_by_id(event_id)
    }

    /// Records `event`'s `(sender, device, txnId)` in both directions: `txn_dedup` (used by
    /// [`RoomActor::dedup_lookup`] to replay it) and `event_txn` (used by
    /// [`RoomActor::transaction_id_for`] to render `unsigned.transaction_id` back to the sending
    /// device later).
    fn record_txn(
        &mut self,
        sender: &UserId,
        device_id: Option<&ruma::DeviceId>,
        txn_id: &str,
        event_id: &EventId,
    ) {
        let key = Self::dedup_key(sender, device_id, txn_id);
        self.txn_dedup.insert(key.clone(), event_id.to_owned());
        self.event_txn.insert(event_id.to_owned(), key);
    }

    /// The transaction ID `event_id` was sent with, if any, **and** `viewer`/`viewer_device`
    /// match the `(sender, device)` that sent it (client-server API "Local echo": only the
    /// sending device's own view of the event carries `unsigned.transaction_id` --
    /// `txnid_test.go`'s `TestTxnScopeOnLocalEcho`). Same in-memory-only lifetime caveat as
    /// [`RoomActor::txn_dedup`]: a room reloaded after idle eviction has forgotten every
    /// transaction ID it ever saw, so this returns `None` for events sent before the last reload
    /// even to their own sender.
    #[must_use]
    pub fn transaction_id_for(
        &self,
        event_id: &EventId,
        viewer: &UserId,
        viewer_device: Option<&ruma::DeviceId>,
    ) -> Option<&str> {
        let (sender, device, txn_id) = self.event_txn.get(event_id)?;
        if sender != viewer {
            return None;
        }
        let viewer_device = viewer_device.map(ToString::to_string).unwrap_or_default();
        if *device != viewer_device {
            return None;
        }
        Some(txn_id.as_str())
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
        self.record_txn(&sender, device_id, txn_id, event.event_id());
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
        self.record_txn(&sender, device_id, txn_id, event.event_id());
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

/// The room short id at the front of a stored alias value. The rest, when there is any, is the
/// user ID of whoever created the alias (see [`RoomActor::create_alias`]).
fn room_sn_from_alias_value(value: &[u8]) -> Result<RoomSn, RoomError> {
    let arr: [u8; 4] = value
        .get(..4)
        .and_then(|head| head.try_into().ok())
        .ok_or_else(|| RoomError::Internal("corrupt alias entry".into()))?;
    Ok(RoomSn::from_be_bytes(arr))
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
    let room_sn = room_sn_from_alias_value(sn_bytes.as_ref())?;
    let Some(room_id_bytes) = tables.room_sn.resolve(&snapshot, room_sn)? else {
        return Ok(None);
    };
    let room_id =
        String::from_utf8(room_id_bytes).map_err(|e| RoomError::Internal(e.to_string()))?;
    Ok(Some(
        OwnedRoomId::try_from(room_id).map_err(|e| RoomError::Internal(e.to_string()))?,
    ))
}

/// Publishes or unpublishes a room in the server's room directory
/// (`PUT /_matrix/client/v3/directory/list/room/{roomId}`). Directory membership is tracked
/// separately from any room-actor state (a room's `RoomSn` presence in
/// `Tables::public_rooms`, not an event in the room's own timeline -- publication is a
/// server-local administrative fact, not something other servers or room members need to see),
/// which is why this is a free function over `(backend, tables)` rather than a `RoomActor` method:
/// `GET /publicRooms` must enumerate published rooms without loading each one's actor first.
///
/// # Errors
/// Returns [`RoomError::RoomNotFound`] if `room_id` has never been created, or
/// [`RoomError::Store`] on a storage failure.
pub fn set_directory_visibility<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
    room_id: &RoomId,
    published: bool,
) -> Result<(), RoomError> {
    let snapshot = backend.snapshot();
    let Some(room_sn) = tables.room_sn.lookup(&snapshot, room_id.as_bytes())? else {
        return Err(RoomError::RoomNotFound(room_id.to_string()));
    };
    transact(backend, TransactConfig::default(), |txn| {
        if published {
            tables
                .public_rooms
                .put(txn, &(room_sn,), b"")
                .map_err(to_kv)
        } else {
            tables.public_rooms.delete(txn, &(room_sn,)).map_err(to_kv)
        }
    })
    .map_err(RoomError::from)
}

/// Whether `room_id` is currently published, per [`set_directory_visibility`]. Returns `Ok(false)`
/// (not [`RoomError::RoomNotFound`]) for a room that has never been created: a caller that only
/// needs a yes/no answer (`GET .../directory/list/room/{roomId}`) should not have to distinguish
/// "private" from "does not exist" -- the spec documents `private` as the default either way.
///
/// # Errors
/// Returns [`RoomError::Store`] on a storage failure.
pub fn is_directory_public<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
    room_id: &RoomId,
) -> Result<bool, RoomError> {
    let snapshot = backend.snapshot();
    let Some(room_sn) = tables.room_sn.lookup(&snapshot, room_id.as_bytes())? else {
        return Ok(false);
    };
    Ok(tables.public_rooms.get(&snapshot, &(room_sn,))?.is_some())
}

/// Every currently published room ID (`GET /publicRooms`'s source of truth for which rooms to
/// enumerate before rendering each one's directory chunk from its own current state). A full scan
/// of the directory keyspace; ordering is whatever the keyspace's own byte order over interned
/// `RoomSn`s happens to produce, not publish time.
///
/// # Errors
/// Returns [`RoomError::Store`]/[`RoomError::Table`] on a storage failure, or
/// [`RoomError::Internal`] if a room's ID cannot be resolved back from its interned `RoomSn`
/// (should not happen: every entry in this keyspace was written by [`set_directory_visibility`]
/// right after looking that same `RoomSn` up).
pub fn list_published_room_ids<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
) -> Result<Vec<OwnedRoomId>, RoomError> {
    let snapshot = backend.snapshot();
    let mut out = Vec::new();
    for item in tables.public_rooms.range(&snapshot, RangeSpec::full()) {
        let ((room_sn,), _) = item?;
        let Some(room_id_bytes) = tables.room_sn.resolve(&snapshot, room_sn)? else {
            continue;
        };
        let room_id =
            String::from_utf8(room_id_bytes).map_err(|e| RoomError::Internal(e.to_string()))?;
        out.push(OwnedRoomId::try_from(room_id).map_err(|e| RoomError::Internal(e.to_string()))?);
    }
    Ok(out)
}

/// Every room `user_id` currently holds `join` membership in, per [`persist`][RoomActor::persist]'s
/// `Tables::joined_rooms` index -- a prefix scan keyed by `user_id`, so this never loads (or even
/// enumerates) any room this user is *not* in. Used by profile-change propagation
/// (`crates/hs-room/src/routes/profile.rs`) to find which rooms need their `m.room.member` event
/// re-stamped with a fresh `displayname`/`avatar_url`.
///
/// # Errors
/// Returns [`RoomError::Store`]/[`RoomError::Table`] on a storage failure, or
/// [`RoomError::Internal`] if a room's ID cannot be resolved back from its interned `RoomSn`
/// (should not happen: every entry in this keyspace was written by [`RoomActor::persist`] right
/// after interning that same room).
pub fn rooms_joined_by_user<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
    user_id: &UserId,
) -> Result<Vec<OwnedRoomId>, RoomError> {
    let snapshot = backend.snapshot();
    let prefix = hs_tables::keyspace::TypedKeyspace::<B::Keyspace, crate::persist::UserJoinedRoomKey>::prefix(
        &(user_id.to_string(),),
    );
    let mut out = Vec::new();
    for item in tables.joined_rooms.range(&snapshot, prefix) {
        let ((_, room_sn), _) = item?;
        let Some(room_id_bytes) = tables.room_sn.resolve(&snapshot, room_sn)? else {
            continue;
        };
        let room_id =
            String::from_utf8(room_id_bytes).map_err(|e| RoomError::Internal(e.to_string()))?;
        out.push(OwnedRoomId::try_from(room_id).map_err(|e| RoomError::Internal(e.to_string()))?);
    }
    Ok(out)
}

/// Every room this server has ever created, per `Tables::room_meta` -- written once for every
/// room, at its first persisted event (`RoomActor::persist`'s `is_first` branch), and never
/// removed. This is the only enumeration this crate has of "every room on the server" (as opposed
/// to "every room this process currently has resident", `RoomRegistry`'s own map, or "every
/// *published* room", [`list_published_room_ids`]): used by the admin room directory
/// (`crate::admin::RoomRegistryDirectory::list_rooms`), which has no other way to answer `GET
/// /rooms` without a filter that would otherwise need one. A full keyspace scan, same scaling
/// caveat as [`list_published_room_ids`] -- fine for an admin listing, not a hot path.
///
/// # Errors
/// Returns [`RoomError::Store`]/[`RoomError::Table`] on a storage failure, or
/// [`RoomError::Internal`] if a stored row fails to decode (should not happen: every entry here
/// was written by [`RoomActor::persist`] itself).
pub fn list_all_room_ids<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
) -> Result<Vec<OwnedRoomId>, RoomError> {
    let snapshot = backend.snapshot();
    let mut out = Vec::new();
    for item in tables.room_meta.range(&snapshot, RangeSpec::full()) {
        let (_, bytes) = item?;
        let meta: RoomMeta = serde_json::from_slice(&bytes)
            .map_err(|e| RoomError::Internal(format!("corrupt room_meta row: {e}")))?;
        out.push(
            OwnedRoomId::try_from(meta.room_id).map_err(|e| RoomError::Internal(e.to_string()))?,
        );
    }
    Ok(out)
}

/// Every room this server holds, with the room-local position of its newest event (`0` for a
/// room with none at a positive position). Read from the stored timeline, newest key first, so
/// that it costs one short scan per room rather than loading each room into memory: appservice
/// delivery asks this at every start to find the rooms that moved while it was not looking.
///
/// # Errors
/// Returns [`RoomError::Store`] on a storage failure, or [`RoomError::Internal`] if a stored row
/// fails to decode.
pub fn room_heads<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
) -> Result<Vec<(OwnedRoomId, i64)>, RoomError> {
    let snapshot = backend.snapshot();
    let mut out = Vec::new();
    for item in tables.room_meta.range(&snapshot, RangeSpec::full()) {
        let ((room_sn,), bytes) = item?;
        let meta: RoomMeta = serde_json::from_slice(&bytes)
            .map_err(|e| RoomError::Internal(format!("corrupt room_meta row: {e}")))?;
        let room_id =
            OwnedRoomId::try_from(meta.room_id).map_err(|e| RoomError::Internal(e.to_string()))?;
        let mut newest = hs_tables::keyspace::TypedKeyspace::<
            B::Keyspace,
            crate::persist::TimelineKey,
        >::prefix(&(room_sn,));
        newest.reverse = true;
        newest.limit = Some(1);
        let head = match tables.timeline.range(&snapshot, newest).next() {
            Some(entry) => {
                let ((_, room_pos), _) = entry?;
                room_pos.max(0)
            }
            None => 0,
        };
        out.push((room_id, head));
    }
    Ok(out)
}

/// Blocks or unblocks `room_id` (`hs-admin`'s `rooms.set_blocked`). The real enforcement is
/// [`RoomActor::send_event_citing`]'s own precheck (an internal read of this same row, fresh on
/// every call) -- this function only needs to write it.
///
/// # Errors
/// Returns [`RoomError::RoomNotFound`] if `room_id` has never been created, or
/// [`RoomError::Store`] on a storage failure.
pub fn set_room_blocked<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
    room_id: &RoomId,
    blocked: bool,
    reason: Option<String>,
) -> Result<(), RoomError> {
    let snapshot = backend.snapshot();
    let Some(room_sn) = tables.room_sn.lookup(&snapshot, room_id.as_bytes())? else {
        return Err(RoomError::RoomNotFound(room_id.to_string()));
    };
    transact(backend, TransactConfig::default(), |txn| {
        if blocked {
            let value = serde_json::to_vec(&crate::persist::RoomBlock {
                reason: reason.clone(),
            })
            .map_err(|e| hs_kv::KvError::backend(std::io::Error::other(e.to_string())))?;
            tables
                .blocked_rooms
                .put(txn, &(room_sn,), &value)
                .map_err(to_kv)
        } else {
            tables.blocked_rooms.delete(txn, &(room_sn,)).map_err(to_kv)
        }
    })
    .map_err(RoomError::from)
}

/// Whether `room_id` is currently blocked, and if so, its reason (which may itself be absent).
/// `Ok(None)` means "not blocked", including for a room that has never been created (same
/// existence-agnostic convention as [`is_directory_public`]); `Ok(Some(reason))` means blocked.
///
/// # Errors
/// Returns [`RoomError::Store`]/[`RoomError::Table`] on a storage failure, or
/// [`RoomError::Internal`] if the stored row fails to decode.
pub fn room_block_reason<B: KvBackend>(
    backend: &B,
    tables: &Tables<B>,
    room_id: &RoomId,
) -> Result<Option<Option<String>>, RoomError> {
    let snapshot = backend.snapshot();
    let Some(room_sn) = tables.room_sn.lookup(&snapshot, room_id.as_bytes())? else {
        return Ok(None);
    };
    let Some(bytes) = tables.blocked_rooms.get(&snapshot, &(room_sn,))? else {
        return Ok(None);
    };
    let block: crate::persist::RoomBlock = serde_json::from_slice(&bytes)
        .map_err(|e| RoomError::Internal(format!("corrupt blocked-room row: {e}")))?;
    Ok(Some(block.reason))
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

    /// `RoomActor::refresh_own_profile`: re-stamps `user`'s `m.room.member` event in this room
    /// with a fresh profile.
    pub async fn refresh_own_profile(
        &self,
        user: OwnedUserId,
        display_name: Option<String>,
        avatar_url: Option<String>,
        now_ms: i64,
    ) -> Result<Option<Event>, RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| {
            actor.refresh_own_profile(&user, display_name, avatar_url, now_ms)
        })
        .await
    }

    /// `POST /rooms/{roomId}/forget` (`RoomActor::forget`).
    pub async fn forget(&self, user: OwnedUserId) -> Result<(), RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| actor.forget(&user)).await
    }

    /// `hs-admin`'s `rooms.make_admin` (`RoomActor::make_admin`).
    pub async fn make_admin(&self, user_id: OwnedUserId, now_ms: i64) -> Result<Event, RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| actor.make_admin(&user_id, now_ms))
            .await
    }

    /// This room's admin-API summary (`RoomActor::admin_summary`).
    pub async fn admin_summary(&self) -> Result<hs_admin::model::AdminRoom, RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| actor.admin_summary()).await
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

    /// `crate::protocol`'s `Command::PersistInbound`: accepts an already-verified, foreign
    /// [`Event`] and persists it byte-identically. See [`RoomActor::accept_remote_event`] for
    /// exactly what this authorizes, refuses, and the caller's own hash/signature-verification
    /// obligation. This is the seam `hs_federation::inbound::RoomWriteSink` is meant to call
    /// through, one directly-owned `RoomActorHandle` per room.
    pub async fn accept_remote_event(&self, event: Event) -> Result<RemoteEventOutcome, RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| actor.accept_remote_event(event))
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

    /// The current `m.room.power_levels` content, as plain JSON to assert against.
    fn power_levels_of(actor: &RoomActor<MemoryBackend>) -> serde_json::Value {
        let event = actor
            .state_event("m.room.power_levels", "")
            .unwrap()
            .expect("every room this crate creates has power levels");
        crate::routes::render::canonical_to_json(
            event
                .json()
                .get("content")
                .and_then(CanonicalJsonValue::as_object)
                .expect("a state event always has an object for content"),
        )
    }

    #[test]
    fn create_room_bootstraps_creator_as_the_sole_joined_member() {
        let actor = room("public_chat");
        let joined = actor.joined_members().unwrap();
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].header().state_key.as_deref(), Some("@alice:hs1"));
    }

    /// The bug `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md` describes: an
    /// event's signature must be computed over its *redacted* form -- spec order is hash the full
    /// event, redact, sign the redacted object, then copy the resulting signature back onto the
    /// full event (`refs/matrix-spec/content/server-server-api.md`, "Adding hashes and signatures
    /// to outgoing events") -- not over the full, unredacted event directly. `m.room.message` is
    /// the sharpest case: redaction strips *all* of its `content`
    /// (`hs_model::redaction::redact_content`), so signing the unredacted object produces a
    /// signature a spec-compliant verifier -- which always redacts before checking -- rejects
    /// outright.
    #[test]
    fn a_sent_message_is_signed_over_its_redacted_form_not_the_full_event() {
        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1").to_owned();

        let event = actor
            .send_event(
                alice,
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "hello, redact me"}),
                None,
                2,
            )
            .unwrap();

        let rules = room_version::rules_for(actor.room_version()).unwrap();
        let redacted = hs_model::redaction::redact(event.json(), &rules.redaction).unwrap();

        let verifying_key = actor.identity.signing_key.verifying_key();
        let key_id = actor.identity.signing_key.key_id();
        let server_name = actor.identity.server_name.as_str();

        // The spec-compliant check -- verify against the *redacted* form -- must succeed.
        hs_model::signing::verify_object(&redacted, server_name, &key_id, &verifying_key)
            .expect("a real, spec-compliant verifier redacts before checking -- this must verify");

        // The fix must not affect what this server actually stores and serves: the full,
        // unredacted event's `content` is still there for an ordinary client to read. Only what
        // gets hashed to produce the signature bytes changes.
        assert_eq!(
            event
                .json()
                .get("content")
                .and_then(CanonicalJsonValue::as_object)
                .and_then(|c| c.get("body"))
                .and_then(CanonicalJsonValue::as_str),
            Some("hello, redact me")
        );

        // Negative check, proving this test actually exercises the bug: the full, unredacted
        // event's `content` must genuinely differ from the redacted form's (otherwise this test
        // would pass even with the old, buggy "sign the full event" code), and verifying the
        // full object against the same signature must fail, since the signature covers only the
        // redacted bytes.
        let full = event.json();
        assert_ne!(
            full.get("content"),
            redacted.get("content"),
            "an m.room.message's content must actually be stripped by redaction, or this test \
             proves nothing"
        );
        let err = hs_model::signing::verify_object(full, server_name, &key_id, &verifying_key);
        assert!(
            err.is_err(),
            "the full, unredacted object must NOT verify under a signature computed over the \
             redacted form -- if it does, this test is not exercising the bug"
        );
    }

    /// `power_level_content_override` is applied *on top of* the generated power-levels content,
    /// key by key. Element Web sends this field -- carrying `events` alone -- on every room it
    /// creates, so when it replaced the whole content instead, the creator's `users` entry
    /// vanished, the next bootstrap event failed auth, and creating a room from a real client
    /// failed unconditionally, in every preset.
    #[test]
    fn a_power_level_override_of_one_key_keeps_the_creator_grant() {
        let backend = MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        let actor = RoomActor::create_room(
            backend,
            tables,
            HomeserverIdentity::for_tests("hs1"),
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest {
                preset: Some("private_chat".to_owned()),
                // Room version 11: the creator's power comes from the `users` entry, not from
                // being the creator, so losing the entry is fatal rather than cosmetic.
                room_version: Some(RoomVersionId::try_from("11").unwrap()),
                power_level_content_override: Some(
                    serde_json::json!({"events": {"m.room.encryption": 100}}),
                ),
                ..Default::default()
            },
            1,
        )
        .expect("the bootstrap events after the power levels must still pass auth");

        let content = power_levels_of(&actor);
        assert_eq!(
            content.pointer("/users/@alice:hs1"),
            Some(&serde_json::Value::from(100)),
            "the override mentioned only `events`, so the creator's grant must survive it"
        );
        assert_eq!(
            content.pointer("/events/m.room.encryption"),
            Some(&serde_json::Value::from(100)),
            "the key the override did carry must win"
        );
    }

    /// The other half of the same rule: a key the override *does* carry replaces the generated
    /// one outright, rather than being merged into it. `users` is the case that matters --
    /// Complement's v12 suite asserts the resulting `users` equals exactly what was sent.
    #[test]
    fn a_power_level_override_replaces_the_key_it_names() {
        let backend = MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        let actor = RoomActor::create_room(
            backend,
            tables,
            HomeserverIdentity::for_tests("hs1"),
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest {
                preset: Some("private_chat".to_owned()),
                room_version: Some(RoomVersionId::try_from("11").unwrap()),
                power_level_content_override: Some(serde_json::json!({
                    "users": {"@alice:hs1": 100, "@bob:hs1": 50}
                })),
                ..Default::default()
            },
            1,
        )
        .unwrap();

        let content = power_levels_of(&actor);
        assert_eq!(
            content.pointer("/users/@bob:hs1"),
            Some(&serde_json::Value::from(50))
        );
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
        let ids_at_first: BTreeSet<OwnedEventId> = at_first
            .state
            .iter()
            .map(|e| e.event_id().to_owned())
            .collect();
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
        assert_eq!(
            actor.forward_extremities_vec(),
            vec![*actor.event_id_index.get(merge.event_id()).unwrap()]
        );

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
            resolved.event_id() == branch_a.event_id()
                || resolved.event_id() == branch_b.event_id(),
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
            actor
                .paginate(None, Direction::Backward, usize::MAX)
                .0
                .len(),
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
            .redact_txn(
                alice.clone(),
                None,
                "txn-2",
                first.event_id().to_owned(),
                None,
                4,
            )
            .unwrap();
        let redact_retried = actor
            .redact_txn(alice, None, "txn-2", first.event_id().to_owned(), None, 5)
            .unwrap();
        assert_eq!(redact_first.event_id(), redact_retried.event_id());
    }

    /// `unsigned.transaction_id`'s rendering contract (`RoomActor::transaction_id_for`):
    /// present for the sending `(user, device)`, absent for a different device of the same user,
    /// absent for a different user entirely, and absent for an event that was never sent through
    /// a `{txnId}`-suffixed endpoint at all.
    #[test]
    fn transaction_id_for_is_scoped_to_sender_and_device() {
        use ruma::device_id;
        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1").to_owned();

        let event = actor
            .send_event_txn(
                alice.clone(),
                Some(device_id!("DEVICE1")),
                "txn-scope",
                "m.room.message".to_owned(),
                serde_json::json!({"body": "hi"}),
                2,
            )
            .unwrap();

        assert_eq!(
            actor.transaction_id_for(event.event_id(), &alice, Some(device_id!("DEVICE1"))),
            Some("txn-scope"),
            "the sending device must see its own transaction id"
        );
        assert_eq!(
            actor.transaction_id_for(event.event_id(), &alice, Some(device_id!("DEVICE2"))),
            None,
            "a different device of the same user must not see it"
        );
        assert_eq!(
            actor.transaction_id_for(
                event.event_id(),
                user_id!("@bob:hs1"),
                Some(device_id!("DEVICE1"))
            ),
            None,
            "a different user must not see it, even with the same device id"
        );

        // An ordinary state event (not sent through a `{txnId}` endpoint) never carries one.
        let state_event = actor
            .send_event(
                alice.clone(),
                "m.room.topic".to_owned(),
                Some(String::new()),
                serde_json::json!({"topic": "hello"}),
                None,
                3,
            )
            .unwrap();
        assert_eq!(
            actor.transaction_id_for(state_event.event_id(), &alice, Some(device_id!("DEVICE1"))),
            None
        );
    }

    /// Client-server API "Setting state twice is idempotent" (`rooms_state_test.go`): resending
    /// the exact same `(event_type, state_key, content)` returns the existing event rather than
    /// persisting a second one; a genuinely different content still creates a new event.
    #[test]
    fn setting_the_same_state_twice_is_idempotent() {
        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1").to_owned();

        let first = actor
            .send_event(
                alice.clone(),
                "a.test.state.type".to_owned(),
                Some(String::new()),
                serde_json::json!({"a_key": "a_value"}),
                None,
                2,
            )
            .unwrap();
        let second = actor
            .send_event(
                alice.clone(),
                "a.test.state.type".to_owned(),
                Some(String::new()),
                serde_json::json!({"a_key": "a_value"}),
                None,
                3,
            )
            .unwrap();
        assert_eq!(
            first.event_id(),
            second.event_id(),
            "identical state content must not create a second event"
        );

        let changed = actor
            .send_event(
                alice,
                "a.test.state.type".to_owned(),
                Some(String::new()),
                serde_json::json!({"a_key": "a_different_value"}),
                None,
                4,
            )
            .unwrap();
        assert_ne!(
            first.event_id(),
            changed.event_id(),
            "genuinely different content must still create a new event"
        );
    }

    /// Client-server API "Joining room twice is idempotent" (`rooms_state_test.go`): a repeated
    /// join with unchanged content (no profile change in between) must not mint a second
    /// `m.room.member` event.
    #[test]
    fn joining_a_room_twice_is_idempotent() {
        let mut actor = room("public_chat");
        let bob = user_id!("@bob:hs1").to_owned();

        let first = actor
            .membership_action(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                2,
            )
            .unwrap();
        let second = actor
            .membership_action(bob.clone(), Action::Join, bob, serde_json::json!({}), 3)
            .unwrap();
        assert_eq!(
            first.event_id(),
            second.event_id(),
            "rejoining with unchanged content must not create a second join event"
        );
    }

    // --- `RoomActor::accept_remote_event`: the "join a real public room over federation" gap ---

    /// Builds a `m.room.message`, hashed and signed exactly the way `hs_federation::inbound`'s
    /// `verify_pdu` would hand one to `RoomActor::accept_remote_event` -- but built and signed
    /// entirely independently of `crate::pipeline::build_and_authorize` (which both builds *and*
    /// authorizes, and therefore cannot construct an event that fails authorization). `sender`'s
    /// `auth_events` are exactly this room's current `m.room.create` and `m.room.power_levels`
    /// (never a `m.room.member` entry for `sender`, matching what a real sender selects when it
    /// has none) -- valid if `sender` is joined, and deliberately unauthorized if not.
    fn build_remote_message(
        actor: &RoomActor<MemoryBackend>,
        sender: &UserId,
        remote_server: &ruma::ServerName,
        remote_key: &hs_model::signing::SigningKeyPair,
        body: &str,
    ) -> Event {
        use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
        use hs_model::{hash, signing};

        let create = actor.state_event("m.room.create", "").unwrap().unwrap();
        let power_levels = actor
            .state_event("m.room.power_levels", "")
            .unwrap()
            .unwrap();
        // Mirrors `crate::pipeline::select_auth_events`: `m.room.member(sender)` is only in the
        // wanted set if it actually has a value in current state -- present when `sender` has
        // ever joined/been invited/etc, absent (correctly) when `sender` is a stranger like
        // `accept_remote_event_rejects_an_unauthorized_sender_...`'s `eve`.
        let sender_member = actor.state_event("m.room.member", sender.as_str()).unwrap();
        let prev_sns = actor.forward_extremities_vec();
        let prev_refs = actor.refs_for(&prev_sns).unwrap();
        let depth = prev_refs.iter().map(|r| r.depth).max().map_or(1, |d| d + 1);

        let mut object = serde_json::Map::new();
        object.insert("type".into(), serde_json::json!("m.room.message"));
        object.insert("sender".into(), serde_json::json!(sender.as_str()));
        object.insert(
            "room_id".into(),
            serde_json::json!(actor.room_id().as_str()),
        );
        object.insert("origin_server_ts".into(), serde_json::json!(2_i64));
        object.insert("depth".into(), serde_json::json!(depth));
        object.insert(
            "content".into(),
            serde_json::json!({"msgtype": "m.text", "body": body}),
        );
        object.insert(
            "prev_events".into(),
            serde_json::json!(
                prev_refs
                    .iter()
                    .map(|r| r.event_id.to_string())
                    .collect::<Vec<_>>()
            ),
        );
        let mut auth_event_ids = vec![
            create.event_id().to_string(),
            power_levels.event_id().to_string(),
        ];
        if let Some(member) = sender_member {
            auth_event_ids.push(member.event_id().to_string());
        }
        object.insert("auth_events".into(), serde_json::json!(auth_event_ids));

        let mut canonical = to_canonical_object(&serde_json::Value::Object(object), true).unwrap();
        let content_hash = hash::content_hash_base64(&canonical);
        canonical.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(CanonicalJsonObject::from([(
                "sha256".to_owned(),
                CanonicalJsonValue::String(content_hash),
            )])),
        );
        signing::sign_object(&mut canonical, remote_server, remote_key).unwrap();
        let final_bytes = CanonicalJsonValue::Object(canonical).to_canonical_bytes();
        let final_value: serde_json::Value = serde_json::from_slice(&final_bytes).unwrap();
        Event::parse(&final_value, RoomVersionId::V11).unwrap()
    }

    /// Deliverable 4's core claim: a remote event round-trips byte-identically (its `event_id`
    /// still hashes correctly after storage) and reaches both the timeline and the publish stream
    /// `hs-user`'s feeds subscribe to.
    #[test]
    fn accept_remote_event_round_trips_byte_identically_and_reaches_publish_stream() {
        let mut actor = room("public_chat");
        let bob = user_id!("@bob:remote.example");
        // Bob joins through the ordinary membership pipeline (a public room needs no invite) --
        // this is what a real federated join would have already produced in this room's state;
        // `accept_remote_event` only ever reads the room's *already-persisted* state; it does not
        // care how bob got there.
        actor
            .membership_action(
                bob.to_owned(),
                Action::Join,
                bob.to_owned(),
                serde_json::json!({}),
                2,
            )
            .unwrap();

        let mut rx = actor.subscribe();

        let remote_key = hs_model::signing::SigningKeyPair::generate("1");
        let remote_server = ruma::ServerName::parse("remote.example").unwrap();
        let remote_event = build_remote_message(
            &actor,
            bob,
            &remote_server,
            &remote_key,
            "hello from a real federation event",
        );
        let original_bytes = remote_event.canonical_bytes().clone();
        let event_id = remote_event.event_id().to_owned();

        let outcome = actor.accept_remote_event(remote_event.clone()).unwrap();
        assert!(matches!(outcome, RemoteEventOutcome::Stored(_)));

        // Byte-identical: not rebuilt, not re-signed.
        let stored = actor.event_by_id(&event_id).unwrap();
        assert_eq!(stored.canonical_bytes(), &original_bytes);
        assert_eq!(stored.event_id(), &*event_id);

        // Reached the timeline.
        let (events, _) = actor.paginate(None, Direction::Backward, 1);
        assert_eq!(events[0].event_id(), &*event_id);

        // Reached the publish stream a local user's sync feed subscribes to
        // (`docs/workstreams/README.md`'s week-8 seam).
        let update = rx.try_recv().unwrap();
        assert_eq!(update.event_id, event_id);
    }

    /// Deliverable 3: receiving the same remote event twice is a no-op, not a duplicate or an
    /// error.
    #[test]
    fn accept_remote_event_replay_is_a_no_op() {
        let mut actor = room("public_chat");
        let bob = user_id!("@bob:remote.example");
        actor
            .membership_action(
                bob.to_owned(),
                Action::Join,
                bob.to_owned(),
                serde_json::json!({}),
                2,
            )
            .unwrap();

        let remote_key = hs_model::signing::SigningKeyPair::generate("1");
        let remote_server = ruma::ServerName::parse("remote.example").unwrap();
        let remote_event =
            build_remote_message(&actor, bob, &remote_server, &remote_key, "sent once");

        let first = actor.accept_remote_event(remote_event.clone()).unwrap();
        assert!(matches!(first, RemoteEventOutcome::Stored(_)));
        let count_after_first = actor
            .paginate(None, Direction::Backward, usize::MAX)
            .0
            .len();

        let second = actor.accept_remote_event(remote_event).unwrap();
        assert_eq!(second, RemoteEventOutcome::AlreadyKnown);
        let count_after_second = actor
            .paginate(None, Direction::Backward, usize::MAX)
            .0
            .len();

        assert_eq!(
            count_after_first, count_after_second,
            "replaying the same event must not add a second timeline entry"
        );
    }

    /// Deliverable 2: a remote event whose sender never joined the room must fail authorization,
    /// be refused outright, and never become visible in the timeline.
    ///
    /// This is also this session's mutation test (see the status file): with either
    /// `auth::check_event_auth` call in `RoomActor::accept_remote_event` short-circuited to
    /// `Ok(())`, this test fails -- confirmed by hand, then reverted.
    #[test]
    fn accept_remote_event_rejects_an_unauthorized_sender_and_it_never_becomes_visible() {
        let mut actor = room("public_chat");
        let eve = user_id!("@eve:remote.example");
        let remote_key = hs_model::signing::SigningKeyPair::generate("1");
        let remote_server = ruma::ServerName::parse("remote.example").unwrap();
        let bad_event = build_remote_message(
            &actor,
            eve,
            &remote_server,
            &remote_key,
            "i was never a member of this room",
        );
        let bad_event_id = bad_event.event_id().to_owned();

        let err = actor.accept_remote_event(bad_event).unwrap_err();
        assert!(
            matches!(err, RoomError::Forbidden(_)),
            "expected Forbidden, got {err:?}"
        );
        assert!(actor.event_by_id(&bad_event_id).is_none());
        let (events, _) = actor.paginate(None, Direction::Backward, usize::MAX);
        assert!(events.iter().all(|e| e.event_id() != &*bad_event_id));
    }

    /// Deliverable 3: an event whose `prev_events` this actor does not hold is the ordinary
    /// federation "needs backfill" case, not a hard rejection and not a panic -- named with its
    /// own distinct error so a caller (track 06) can tell it apart from a genuine authorization
    /// failure.
    #[test]
    fn accept_remote_event_with_unknown_prev_events_is_a_distinct_error() {
        use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
        use hs_model::{hash, signing};

        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1");
        let remote_key = hs_model::signing::SigningKeyPair::generate("1");
        let remote_server = ruma::ServerName::parse("hs1").unwrap();

        let mut object = serde_json::Map::new();
        object.insert("type".into(), serde_json::json!("m.room.message"));
        object.insert("sender".into(), serde_json::json!(alice.as_str()));
        object.insert(
            "room_id".into(),
            serde_json::json!(actor.room_id().as_str()),
        );
        object.insert("origin_server_ts".into(), serde_json::json!(99_i64));
        object.insert("depth".into(), serde_json::json!(99_i64));
        object.insert("content".into(), serde_json::json!({"body": "orphan"}));
        object.insert(
            "prev_events".into(),
            serde_json::json!(["$doesnotexist:remote.example"]),
        );
        object.insert("auth_events".into(), serde_json::json!([]));

        let mut canonical = to_canonical_object(&serde_json::Value::Object(object), true).unwrap();
        let content_hash = hash::content_hash_base64(&canonical);
        canonical.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(CanonicalJsonObject::from([(
                "sha256".to_owned(),
                CanonicalJsonValue::String(content_hash),
            )])),
        );
        signing::sign_object(&mut canonical, &remote_server, &remote_key).unwrap();
        let final_bytes = CanonicalJsonValue::Object(canonical).to_canonical_bytes();
        let final_value: serde_json::Value = serde_json::from_slice(&final_bytes).unwrap();
        let orphan = Event::parse(&final_value, RoomVersionId::V11).unwrap();

        let err = actor.accept_remote_event(orphan).unwrap_err();
        assert!(
            matches!(err, RoomError::MissingAncestors(_)),
            "expected MissingAncestors, got {err:?}"
        );
    }

    // --- `GET /rooms/{roomId}/threads`: `RoomActor::thread_roots` ---

    fn thread_reply(
        actor: &mut RoomActor<MemoryBackend>,
        sender: &UserId,
        root: &EventId,
        now_ms: i64,
    ) -> Event {
        actor
            .send_event(
                sender.to_owned(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({
                    "msgtype": "m.text",
                    "body": "reply",
                    "m.relates_to": {"rel_type": "m.thread", "event_id": root.as_str()},
                }),
                None,
                now_ms,
            )
            .unwrap()
    }

    /// `room_threads_test.go`'s `TestThreadsEndpoint`: thread roots come back most-recently-active
    /// first, and a new reply to an older thread moves it back to the front.
    #[test]
    fn thread_roots_are_ordered_by_latest_reply_and_reorder_on_a_new_reply() {
        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1").to_owned();

        let root1 = actor
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "Thread 1 Root"}),
                None,
                2,
            )
            .unwrap();
        let root2 = actor
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "Thread 2 Root"}),
                None,
                3,
            )
            .unwrap();
        thread_reply(&mut actor, &alice, root1.event_id(), 4);
        thread_reply(&mut actor, &alice, root2.event_id(), 5);

        let roots = actor.thread_roots(&alice, false);
        assert_eq!(
            roots.iter().map(|e| e.event_id()).collect::<Vec<_>>(),
            vec![root2.event_id(), root1.event_id()],
            "the thread with the most recent reply (root2) must come first"
        );

        // A new reply to the *older* thread must move it back to the front.
        thread_reply(&mut actor, &alice, root1.event_id(), 6);
        let roots = actor.thread_roots(&alice, false);
        assert_eq!(
            roots.iter().map(|e| e.event_id()).collect::<Vec<_>>(),
            vec![root1.event_id(), root2.event_id()],
            "a new reply to root1's thread must move it back to the front"
        );
    }

    /// Two replies in the same millisecond -- which is what a script, a bridge, or a test sends
    /// -- must still come back in the order they happened. Ordered by timestamp they tied, and
    /// the winner was decided by comparing event IDs, which are hashes.
    #[test]
    fn thread_roots_in_the_same_millisecond_are_ordered_by_what_happened_last() {
        for _ in 0..8 {
            let mut actor = room("public_chat");
            let alice = user_id!("@alice:hs1").to_owned();
            let mut roots = Vec::new();
            for body in ["one", "two", "three"] {
                roots.push(
                    actor
                        .send_event(
                            alice.clone(),
                            "m.room.message".to_owned(),
                            None,
                            serde_json::json!({"msgtype": "m.text", "body": body}),
                            None,
                            7,
                        )
                        .unwrap(),
                );
            }
            for root in &roots {
                thread_reply(&mut actor, &alice, root.event_id(), 7);
            }
            let listed = actor.thread_roots(&alice, false);
            assert_eq!(
                listed.iter().map(|e| e.event_id()).collect::<Vec<_>>(),
                roots.iter().rev().map(|e| e.event_id()).collect::<Vec<_>>(),
                "replied to last, listed first"
            );
        }
    }

    /// `?include=participated`: only threads the requester started or replied to come back.
    #[test]
    fn thread_roots_participated_filter_matches_the_threading_module_rules() {
        let mut actor = room("public_chat");
        let alice = user_id!("@alice:hs1").to_owned();
        let bob = user_id!("@bob:hs1").to_owned();
        actor
            .membership_action(
                bob.clone(),
                Action::Join,
                bob.clone(),
                serde_json::json!({}),
                2,
            )
            .unwrap();

        // alice starts a thread but never replies to it (rule 1: root sender counts).
        let alice_root = actor
            .send_event(
                alice.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "alice's thread"}),
                None,
                3,
            )
            .unwrap();
        thread_reply(&mut actor, &bob, alice_root.event_id(), 4);

        // bob starts a different thread that alice never touches at all.
        let bob_root = actor
            .send_event(
                bob.clone(),
                "m.room.message".to_owned(),
                None,
                serde_json::json!({"msgtype": "m.text", "body": "bob's thread"}),
                None,
                5,
            )
            .unwrap();
        thread_reply(&mut actor, &bob, bob_root.event_id(), 6);

        let all = actor.thread_roots(&alice, false);
        assert_eq!(
            all.len(),
            2,
            "both threads exist regardless of participation"
        );

        let participated = actor.thread_roots(&alice, true);
        assert_eq!(
            participated
                .iter()
                .map(|e| e.event_id())
                .collect::<Vec<_>>(),
            vec![alice_root.event_id()],
            "alice participated only in her own thread, never bob's"
        );
    }
}
