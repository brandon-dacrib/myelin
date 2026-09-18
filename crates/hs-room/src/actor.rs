//! [`RoomActor`]: the synchronous, single-room state machine. [`RoomActorHandle`]: the async,
//! serialized mailbox wrapping it. See `crate::protocol` for the design rationale.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use hs_kv::{KvBackend, TransactConfig, transact};
use hs_model::Event;
use hs_model::ids::{EventSn, RoomSn};
use hs_model::room_version::{self, RoomIdFormat, RoomVersionRules};
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, RoomVersionId, UserId,
};

use crate::error::RoomError;
use crate::identity::HomeserverIdentity;
use crate::membership::{self, Action, PriorState};
use crate::persist::{PersistedEvent, RoomMeta, Tables};
use crate::pipeline::{self, CurrentState, NewEvent};
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
    /// Every event body held in memory. Phase 0 scope: unbounded (the whole room's history stays
    /// resident for the actor's lifetime); see `crate::registry` for room-granularity eviction and
    /// this module's doc comment on `events` for the documented next step (a bounded recent-window
    /// cache with KV fallback for older events).
    events: HashMap<EventSn, Event>,
    event_id_index: HashMap<OwnedEventId, EventSn>,
    /// `(event_type, state_key) -> EventSn`: the room's current, resolved state.
    current_state: BTreeMap<(String, String), EventSn>,
    /// The room's sole forward extremity. Always `Some` after the first event; stays a single
    /// value because this actor is the room's only writer and always sets a new event's
    /// `prev_events` to exactly its own last-persisted event -- see `crate::pipeline`'s module
    /// docs for why that means state resolution is never triggered by local-only operation.
    forward_extremity: Option<EventSn>,
    /// `room_pos -> EventSn`, ascending.
    timeline: BTreeMap<i64, EventSn>,
    next_room_pos: i64,
    /// `target_event_id -> [child EventSn]`, insertion order, for `crate::relations`.
    relations_by_target: HashMap<OwnedEventId, Vec<EventSn>>,
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
    /// # Errors
    /// Returns [`RoomError::UnsupportedRoomVersion`] if `room_version` is unknown, or if it uses
    /// hash-based room IDs (room version 12 and later, MSC4291) -- not implemented in this pass;
    /// see `docs/rfcs/0010-room-actor-state-store-seam.md`. Otherwise, any error
    /// [`RoomActor::send_event`] can return.
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
        if rules.room_id_format != RoomIdFormat::V1Opaque {
            return Err(RoomError::UnsupportedRoomVersion(format!(
                "{}: hash-based room IDs (room version 12+) are not implemented",
                room_version.as_str()
            )));
        }
        let room_sn = Self::intern_room(&backend, &tables, &room_id)?;
        let (publish, _rx) = tokio::sync::broadcast::channel(64);
        let mut actor = Self {
            backend,
            tables,
            identity,
            room_sn,
            room_id: room_id.clone(),
            room_version: room_version.clone(),
            rules,
            events: HashMap::new(),
            event_id_index: HashMap::new(),
            current_state: BTreeMap::new(),
            forward_extremity: None,
            timeline: BTreeMap::new(),
            next_room_pos: 1,
            relations_by_target: HashMap::new(),
            publish,
        };
        actor.send_event(
            creator,
            "m.room.create".to_owned(),
            Some(String::new()),
            creation_content,
            None,
            now_ms,
        )?;
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

        let (publish, _rx) = tokio::sync::broadcast::channel(64);
        let mut actor = Self {
            backend,
            tables,
            identity,
            room_sn,
            room_id: room_id.to_owned(),
            room_version: room_version.clone(),
            rules,
            events: HashMap::new(),
            event_id_index: HashMap::new(),
            current_state: BTreeMap::new(),
            forward_extremity: None,
            timeline: BTreeMap::new(),
            next_room_pos: 1,
            relations_by_target: HashMap::new(),
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
            actor.absorb_loaded_event(event_sn, event, room_pos);
        }

        Ok(Some(actor))
    }

    fn absorb_loaded_event(&mut self, event_sn: EventSn, event: Event, room_pos: i64) {
        if let Some(state_key) = event.header().state_key.clone() {
            self.current_state
                .insert((event.header().event_type.clone(), state_key), event_sn);
        }
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
        self.forward_extremity = Some(event_sn);
        self.events.insert(event_sn, event);
    }

    fn current_view(&self) -> CurrentState<'_> {
        CurrentState {
            state: &self.current_state,
            events: &self.events,
        }
    }

    fn forward_refs(&self) -> Result<Vec<pipeline::EventRef>, RoomError> {
        match self.forward_extremity {
            None => Ok(Vec::new()),
            Some(sn) => {
                let event = self.events.get(&sn).ok_or_else(|| {
                    RoomError::Internal("forward extremity not in hot cache".into())
                })?;
                Ok(vec![pipeline::event_ref(event, &self.rules)?])
            }
        }
    }

    /// Builds, hashes, signs, authorizes and persists a new locally-originated event.
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
        let prev_events = self.forward_refs()?;
        let event = pipeline::build_and_authorize(
            &self.room_version,
            &self.rules,
            &self.room_id,
            &self.identity.server_name,
            &self.identity.signing_key,
            now_ms,
            &prev_events,
            self.current_view(),
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
        let prior = self.prior_membership(&target);
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

    fn prior_membership(&self, target: &UserId) -> PriorState {
        let Some(sn) = self
            .current_state
            .get(&("m.room.member".to_owned(), target.to_string()))
        else {
            return PriorState::None;
        };
        let Some(event) = self.events.get(sn) else {
            return PriorState::None;
        };
        let value = event
            .json()
            .get("content")
            .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
            .and_then(|c| c.get("membership"))
            .and_then(hs_model::canonical::CanonicalJsonValue::as_str);
        match value {
            Some("join") => PriorState::Join,
            Some("invite") => PriorState::Invite,
            Some("leave") => PriorState::Leave,
            Some("ban") => PriorState::Ban,
            Some("knock") => PriorState::Knock,
            _ => PriorState::None,
        }
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
        let old_extremity = self.forward_extremity;
        let event_id_bytes = event.event_id().as_bytes().to_vec();

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
            if let Some(old) = old_extremity {
                self.tables
                    .extremities_fwd
                    .delete(txn, &(room_sn, old))
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
            self.current_state
                .insert((event.header().event_type.clone(), state_key), event_sn);
        }
        if let Some(rel) = relation {
            self.relations_by_target
                .entry(rel.target)
                .or_default()
                .push(event_sn);
        }
        self.forward_extremity = Some(event_sn);
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
                    users.insert(creator.to_string(), serde_json::Value::from(100));
                    if preset == "trusted_private_chat" {
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
    #[must_use]
    pub fn state_event(&self, event_type: &str, state_key: &str) -> Option<&Event> {
        self.current_view().event_for(event_type, state_key)
    }

    /// Every current-state event.
    #[must_use]
    pub fn full_state(&self) -> Vec<&Event> {
        self.current_state
            .values()
            .filter_map(|sn| self.events.get(sn))
            .collect()
    }

    /// One event by ID, if this actor holds it (its own room's events only).
    #[must_use]
    pub fn event_by_id(&self, event_id: &EventId) -> Option<&Event> {
        let sn = self.event_id_index.get(event_id)?;
        self.events.get(sn)
    }

    /// Every current `m.room.member` event.
    #[must_use]
    pub fn members(&self) -> Vec<&Event> {
        self.current_state
            .iter()
            .filter(|((t, _), _)| t == "m.room.member")
            .filter_map(|(_, sn)| self.events.get(sn))
            .collect()
    }

    /// Every current `m.room.member` event whose `membership` is `join`.
    #[must_use]
    pub fn joined_members(&self) -> Vec<&Event> {
        self.members()
            .into_iter()
            .filter(|e| {
                e.json()
                    .get("content")
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                    .and_then(|c| c.get("membership"))
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                    == Some("join")
            })
            .collect()
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
    /// effect to the target if accepted.
    pub async fn redact(
        &self,
        sender: OwnedUserId,
        target: OwnedEventId,
        reason: Option<String>,
        now_ms: i64,
    ) -> Result<Event, RoomError>
    where
        B: 'static,
    {
        self.with_actor(move |actor| {
            let mut content = serde_json::json!({});
            if let Some(reason) = &reason {
                content["reason"] = serde_json::Value::String(reason.clone());
            }
            let event = actor.send_event(
                sender,
                "m.room.redaction".to_owned(),
                None,
                content,
                Some(target.clone()),
                now_ms,
            )?;
            actor.apply_redaction(&target)?;
            Ok(event)
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
        let joined = actor.joined_members();
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
        let original_state_count = actor.full_state().len();
        drop(actor);

        let reloaded = RoomActor::load(backend, tables, identity, &room_id)
            .unwrap()
            .expect("room was persisted, load must find it");
        assert_eq!(reloaded.room_id(), &*room_id);
        assert_eq!(reloaded.full_state().len(), original_state_count);
        assert_eq!(
            reloaded
                .state_event("m.room.name", "")
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

    #[test]
    fn room_version_12_hash_based_room_ids_are_a_documented_gap() {
        let backend = MemoryBackend::new();
        let tables = Tables::open(&backend).unwrap();
        let identity = HomeserverIdentity::for_tests("hs1");
        let result = RoomActor::create_room(
            backend,
            tables,
            identity,
            user_id!("@alice:hs1").to_owned(),
            CreateRoomRequest {
                room_version: Some(RoomVersionId::V12),
                ..Default::default()
            },
            1,
        );
        match result {
            Err(RoomError::UnsupportedRoomVersion(_)) => {}
            Ok(_) => panic!("expected UnsupportedRoomVersion, got Ok"),
            Err(other) => panic!("expected UnsupportedRoomVersion, got {other}"),
        }
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

            prop_assert_eq!(actor.joined_members().len(), 1);

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
            prop_assert_eq!(actor.joined_members().len(), 2);
        }
    }
}
