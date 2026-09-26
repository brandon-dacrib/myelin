//! `make_join`/`send_join`: the join handshake for a room this server hosts, called by the server
//! of a user who wants to join it.
//!
//! # Scope
//!
//! - **`make_join` is fully real**: it builds an unsigned join template against this room's
//!   current state, with real `prev_events`/`depth` (from
//!   [`crate::room_source::RoomDataSource::forward_extremities`]) and real `auth_events`
//!   (selected via [`hs_state::auth::expected_auth_types`] against current state, exactly the
//!   algorithm the spec names -- not a re-derivation of it).
//! - **`send_join` validates for real**: signature and content-hash verification
//!   ([`crate::inbound::verify_pdu`]), shape checks (it is an `m.room.member` event for the right
//!   room with `membership: join`, signed by the domain named in its own `sender`), and
//!   authorization against current state ([`hs_state::auth::check_event_auth`]) -- the
//!   state-*dependent* half of the spec's checks. It does **not** run
//!   [`hs_state::auth::check_auth_events_selection`] (the state-*independent* half) or the spec's
//!   required three-snapshot check (implied-by-`auth_events`, before-the-event,
//!   current-at-receipt): `hs-room`'s own event pipeline documents this same gap as "still track
//!   06's job" and out of scope for a locally-authored event, and it is out of scope here too --
//!   see the status file.
//! - **Persistence is the same honest gap `crate::inbound` documents**: `send_join` can only ever
//!   report success for a join event this server already holds (the idempotent-replay case);
//!   `hs-room` has no API to accept a foreign server's already-signed event. See
//!   [`crate::inbound::RoomWriteSink`] and the status file's RFC.
//! - **Faster joins (MSC3706/MSC4229) are out of scope.** Every response here is the full,
//!   unabridged state and auth chain; `members_omitted` is always `false`.
//! - **Forward extremities are assumed to number exactly one.** Nothing that can currently reach
//!   this server's rooms introduces a second one (only `RoomActor::send_event_citing`, used by
//!   this workspace's own fork-representing tests, can -- see its doc comment). A future inbound
//!   ingestion path that lands events with divergent `prev_events` would need this assumption
//!   revisited, at which point `make_join`'s `prev_events` should cite every current extremity, not
//!   just the one this session's `RoomDataSource::forward_extremities` implementations happen to
//!   return.
//! - **`EventsReferenceFormat::V1WithHash` room versions (1 and 2) are not supported by this
//!   module**: computing a `[event_id, {"sha256": ...}]` reference pair for a forward extremity
//!   needs that event's full body, which `forward_extremities` does not carry (only
//!   `(event_id, depth)`). This server's own default room version is 11 (`V2IdOnly`, bare event
//!   ID strings), so this is a narrow, explicitly-declared gap, not a silent one.

use std::collections::HashMap;

use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use hs_model::room_version::EventsReferenceFormat;
use hs_state::auth::{self, IncomingEvent};
use hs_state::state_fetch::{StateEntry, StateFetch};
use ruma::{OwnedUserId, RoomId, RoomVersionId, UserId};
use serde_json::Value;

use crate::inbound::{RoomWriteSink, WriteOutcome, event_json, verify_pdu};
use crate::keys::DynRemoteKeyCache;
use crate::room_source::{RoomDataSource, RoomSourceError};
use crate::sender::OutboundPduSink;

/// Why a join handshake call failed.
#[derive(Debug, Clone)]
pub enum JoinError {
    RoomNotFound,
    UnsupportedRoomVersion(String),
    IncompatibleRoomVersion {
        room_version: String,
    },
    MalformedUserId(String),
    MalformedEvent(String),
    RoomIdMismatch,
    SenderServerMismatch {
        sender_server: String,
        origin: String,
    },
    NotAuthorized(String),
    Store(String),
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RoomNotFound => write!(f, "unknown room"),
            Self::UnsupportedRoomVersion(v) => write!(f, "unsupported room version {v}"),
            Self::IncompatibleRoomVersion { room_version } => write!(
                f,
                "the room's version ({room_version}) is not in the requester's supported list"
            ),
            Self::MalformedUserId(e) => write!(f, "malformed user id: {e}"),
            Self::MalformedEvent(e) => write!(f, "malformed join event: {e}"),
            Self::RoomIdMismatch => write!(f, "event's room_id does not match the request path"),
            Self::SenderServerMismatch {
                sender_server,
                origin,
            } => write!(
                f,
                "event sender's server ({sender_server}) does not match the requesting server ({origin})"
            ),
            Self::NotAuthorized(e) => write!(f, "join not authorized: {e}"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

fn source_err(e: RoomSourceError) -> JoinError {
    match e {
        RoomSourceError::RoomNotFound => JoinError::RoomNotFound,
        RoomSourceError::NotVisible | RoomSourceError::NotFound => {
            JoinError::Store("room state unavailable".to_owned())
        }
    }
}

/// A flat lookup of `(event_type, state_key) -> (sender, content, event_id)`, built once from
/// [`crate::room_source::StateForJoin::state`] and used both for the spec's auth-events selection
/// algorithm (which needs event IDs) and as a [`StateFetch`] for
/// [`hs_state::auth::check_event_auth`] (which needs sender/content).
struct FlatState {
    by_key: HashMap<(String, String), (OwnedUserId, CanonicalJsonObject, String)>,
}

impl FlatState {
    fn build(state: &[(String, Value)]) -> Self {
        let mut by_key = HashMap::new();
        for (event_id, event) in state {
            let Some(event_type) = event.get("type").and_then(Value::as_str) else {
                continue;
            };
            let Some(state_key) = event.get("state_key").and_then(Value::as_str) else {
                continue;
            };
            let Some(sender_str) = event.get("sender").and_then(Value::as_str) else {
                continue;
            };
            let Ok(sender) = UserId::parse(sender_str) else {
                continue;
            };
            let content = event
                .get("content")
                .cloned()
                .unwrap_or(Value::Object(serde_json::Map::new()));
            let Ok(content) = to_canonical_object(&content, false) else {
                continue;
            };
            by_key.insert(
                (event_type.to_owned(), state_key.to_owned()),
                (sender, content, event_id.clone()),
            );
        }
        Self { by_key }
    }

    fn event_id_for(&self, event_type: &str, state_key: &str) -> Option<&str> {
        self.by_key
            .get(&(event_type.to_owned(), state_key.to_owned()))
            .map(|(_, _, id)| id.as_str())
    }
}

impl StateFetch for FlatState {
    fn get(&self, event_type: &str, state_key: &str) -> Option<StateEntry<'_>> {
        let (sender, content, _) = self
            .by_key
            .get(&(event_type.to_owned(), state_key.to_owned()))?;
        Some(StateEntry {
            sender: sender.as_ref(),
            content,
        })
    }
}

/// One entry in `prev_events`/`auth_events`, encoded per the room version's reference format. Only
/// [`EventsReferenceFormat::V2IdOnly`] is supported -- see the module doc.
fn encode_ref(event_id: &str, format: EventsReferenceFormat) -> Result<Value, JoinError> {
    match format {
        EventsReferenceFormat::V2IdOnly => Ok(Value::String(event_id.to_owned())),
        EventsReferenceFormat::V1WithHash => Err(JoinError::UnsupportedRoomVersion(
            "room versions 1-2 (V1WithHash event references) are not supported by the join handshake".to_owned(),
        )),
    }
}

/// An unsigned join template, as `make_join` returns it.
#[derive(Debug, Clone)]
pub struct JoinTemplate {
    pub event: Value,
    pub room_version: String,
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

/// Builds an unsigned join event template for `user_id` to join `room_id`, against this server's
/// current state. `supported_versions` is the requester's `?ver=` list (empty means "no
/// preference stated", which every version satisfies, matching the spec's default).
///
/// # Errors
/// See [`JoinError`].
pub async fn make_join(
    rooms: &dyn RoomDataSource,
    room_id: &str,
    user_id: &str,
    supported_versions: &[String],
) -> Result<JoinTemplate, JoinError> {
    let Some(room_version_str) = rooms.room_version(room_id).await else {
        return Err(JoinError::RoomNotFound);
    };
    if !supported_versions.is_empty() && !supported_versions.iter().any(|v| v == &room_version_str)
    {
        return Err(JoinError::IncompatibleRoomVersion {
            room_version: room_version_str,
        });
    }
    let room_version = RoomVersionId::try_from(room_version_str.as_str())
        .map_err(|_| JoinError::UnsupportedRoomVersion(room_version_str.clone()))?;
    let rules = hs_model::room_version::rules_for(&room_version)
        .ok_or_else(|| JoinError::UnsupportedRoomVersion(room_version_str.clone()))?;

    let user = UserId::parse(user_id).map_err(|e| JoinError::MalformedUserId(e.to_string()))?;
    let parsed_room_id = RoomId::parse(room_id).map_err(|_| JoinError::RoomNotFound)?;

    let extremities = rooms
        .forward_extremities(room_id)
        .await
        .map_err(source_err)?;
    if extremities.is_empty() {
        return Err(JoinError::Store(
            "room has no forward extremities".to_owned(),
        ));
    }
    let depth = extremities.iter().map(|(_, d)| *d).max().unwrap_or(0) + 1;
    let prev_events = extremities
        .iter()
        .map(|(id, _)| encode_ref(id, rules.events_reference_format))
        .collect::<Result<Vec<_>, _>>()?;

    let state = rooms.state_for_join(room_id).await.map_err(source_err)?;
    let flat = FlatState::build(&state.state);

    let content = serde_json::json!({ "membership": "join" });
    let content_canonical = to_canonical_object(&content, rules.strict_canonical_json)
        .map_err(|e| JoinError::MalformedEvent(e.to_string()))?;
    let incoming = IncomingEvent {
        event_type: "m.room.member",
        sender: &user,
        room_id: Some(&parsed_room_id),
        state_key: Some(user.as_str()),
        content: &content_canonical,
        prev_event_count: extremities.len(),
        only_prev_event_is_room_create: false,
        event_id: None,
        redacts: None,
    };
    let wanted_auth_types = auth::expected_auth_types(&incoming, &rules)
        .map_err(|e| JoinError::NotAuthorized(e.to_string()))?;
    let mut auth_events = Vec::new();
    for (event_type, state_key) in &wanted_auth_types {
        if let Some(id) = flat.event_id_for(event_type, state_key) {
            auth_events.push(encode_ref(id, rules.events_reference_format)?);
        }
    }

    // A best-effort early check: authorizing the template we are about to hand out against
    // *current* state, so a request that is hopeless (banned user, invite-only room with no
    // invite) gets a clear rejection now rather than a template the eventual `send_join` will
    // reject anyway. This is not a substitute for `send_join`'s own check -- state can move
    // between the two calls -- it is a courtesy.
    auth::check_event_auth(&rules, &incoming, &flat)
        .map_err(|e| JoinError::NotAuthorized(e.to_string()))?;

    let event = serde_json::json!({
        "type": "m.room.member",
        "room_id": room_id,
        "sender": user.as_str(),
        "state_key": user.as_str(),
        "content": content,
        "origin_server_ts": now_ms(),
        "prev_events": prev_events,
        "auth_events": auth_events,
        "depth": depth,
    });

    Ok(JoinTemplate {
        event,
        room_version: room_version_str,
    })
}

/// The result of a successful `send_join`.
#[derive(Debug, Clone)]
pub struct SendJoinResult {
    /// The room's full current state.
    pub state: Vec<Value>,
    /// That state's auth chain.
    pub auth_chain: Vec<Value>,
    /// The verified join event, exactly as submitted (echoed back for v2's `event` field). See the
    /// module doc: this is *not* necessarily what ends up durably stored, since persistence for a
    /// genuinely new event is not yet possible.
    pub event: Value,
    /// Always `false`: faster joins are out of scope.
    pub members_omitted: bool,
}

/// Validates and (to the extent [`RoomWriteSink`] allows) applies a signed join event returned by
/// a remote server after `make_join`.
///
/// A join this call newly stores is also handed to `forward` (if any) for every server with a
/// joined member in the room other than `origin` (which sent it) and `own_server_name` (which is
/// applying it): the spec's requirement that the resident server "send the new join event to all
/// other servers in the room", which is the only way they learn of the new member. A replayed
/// join (`WriteOutcome::AlreadyKnown`) was forwarded the first time and is not forwarded again.
///
/// # Errors
/// See [`JoinError`].
#[allow(clippy::too_many_arguments)]
pub async fn send_join(
    rooms: &dyn RoomDataSource,
    sink: &dyn RoomWriteSink,
    key_cache: &DynRemoteKeyCache,
    room_id: &str,
    event_id: &str,
    signed_event: &Value,
    origin: &str,
    own_server_name: &str,
    forward: Option<&dyn OutboundPduSink>,
) -> Result<SendJoinResult, JoinError> {
    let Some(room_version_str) = rooms.room_version(room_id).await else {
        return Err(JoinError::RoomNotFound);
    };
    let room_version = RoomVersionId::try_from(room_version_str.as_str())
        .map_err(|_| JoinError::UnsupportedRoomVersion(room_version_str.clone()))?;
    let rules = hs_model::room_version::rules_for(&room_version)
        .ok_or_else(|| JoinError::UnsupportedRoomVersion(room_version_str.clone()))?;

    let event = verify_pdu(signed_event, &room_version, key_cache)
        .await
        .map_err(|e| JoinError::MalformedEvent(e.to_string()))?;

    if event.event_id().as_str() != event_id {
        return Err(JoinError::MalformedEvent(
            "event id in the request path does not match the submitted event".to_owned(),
        ));
    }
    if event.header().event_type != "m.room.member" {
        return Err(JoinError::MalformedEvent(
            "not an m.room.member event".to_owned(),
        ));
    }
    let membership = event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .and_then(|c| c.get("membership"))
        .and_then(CanonicalJsonValue::as_str);
    if membership != Some("join") {
        return Err(JoinError::MalformedEvent(
            "content.membership is not \"join\"".to_owned(),
        ));
    }
    let event_room_id = event
        .json()
        .get("room_id")
        .and_then(CanonicalJsonValue::as_str);
    if event_room_id != Some(room_id) {
        return Err(JoinError::RoomIdMismatch);
    }
    let sender_server = event.header().sender.server_name().as_str();
    if sender_server != origin {
        return Err(JoinError::SenderServerMismatch {
            sender_server: sender_server.to_owned(),
            origin: origin.to_owned(),
        });
    }

    let state = rooms.state_for_join(room_id).await.map_err(source_err)?;
    let flat = FlatState::build(&state.state);

    let content = event
        .json()
        .get("content")
        .and_then(CanonicalJsonValue::as_object)
        .cloned()
        .unwrap_or_default();
    let parsed_room_id = RoomId::parse(room_id).map_err(|_| JoinError::RoomNotFound)?;
    let incoming = IncomingEvent {
        event_type: "m.room.member",
        sender: AsRef::<UserId>::as_ref(&event.header().sender),
        room_id: Some(&parsed_room_id),
        state_key: event.header().state_key.as_deref(),
        content: &content,
        prev_event_count: 1,
        only_prev_event_is_room_create: false,
        event_id: Some(event.event_id()),
        redacts: None,
    };
    auth::check_event_auth(&rules, &incoming, &flat)
        .map_err(|e| JoinError::NotAuthorized(e.to_string()))?;

    let value = event_json(&event);
    let outcome = sink
        .accept_verified_event(room_id, event.event_id().as_str(), &value)
        .await
        .map_err(|e| JoinError::Store(e.error))?;
    tracing::info!(room_id, event_id, ?outcome, "send_join: event accepted");

    // Stored vs. AlreadyKnown does not change the response shape, only whether the room's other
    // servers still need telling. Member servers are read after the store, so a room whose only
    // members are the creator and this joiner yields nothing to forward rather than a stale list.
    if let (WriteOutcome::Stored, Some(forward)) = (outcome, forward) {
        let destinations: Vec<String> = rooms
            .member_servers(room_id)
            .await
            .into_iter()
            .filter(|server| server != origin && server != own_server_name)
            .collect();
        if !destinations.is_empty() {
            tracing::debug!(
                room_id,
                event_id,
                servers = destinations.len(),
                "send_join: forwarding the accepted join to the room's other servers"
            );
            forward.enqueue_pdu(destinations, value.clone());
        }
    }

    Ok(SendJoinResult {
        state: state.state.into_iter().map(|(_, v)| v).collect(),
        auth_chain: state.auth_chain,
        event: value,
        members_omitted: false,
    })
}

// `WriteOutcome` is re-exported here purely so callers of this module do not also need to import
// `crate::inbound` just to name the type in a `match`/log statement above.
pub use crate::inbound::WriteOutcome as JoinWriteOutcome;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::StaticWriteSink;
    use crate::keys::{
        KeyServerFetcher, OwnSigningKeys, RemoteKeyCache, build_server_key_response,
    };
    use crate::room_source::{FakeRoom, InMemoryRoomSource};
    use async_trait::async_trait;
    use hs_model::signing::sign_object;

    struct FixedFetcher(Value);
    #[async_trait]
    impl KeyServerFetcher for FixedFetcher {
        async fn fetch_server_key(&self, _server_name: &str) -> Option<Value> {
            Some(self.0.clone())
        }
    }

    /// A minimal, self-consistent room: one `m.room.create` and one creator join, both "signed" by
    /// `resident.example.org` (this server, the room's host) so `FlatState` and auth checks have
    /// something real to look at. Returns `(rooms, room_id, room_version)`.
    fn room_with_creator() -> (InMemoryRoomSource, String, String) {
        room_with_creator_and_servers(Vec::new())
    }

    /// [`room_with_creator`], with `joined_servers` as the fake's `member_servers` answer.
    fn room_with_creator_and_servers(
        joined_servers: Vec<String>,
    ) -> (InMemoryRoomSource, String, String) {
        let room_id = "!r:resident.example.org".to_string();
        let create = serde_json::json!({
            "event_id": "$create",
            "type": "m.room.create",
            "room_id": room_id,
            "sender": "@creator:resident.example.org",
            "state_key": "",
            "content": {"creator": "@creator:resident.example.org", "room_version": "11"},
        });
        let power_levels = serde_json::json!({
            "event_id": "$power",
            "type": "m.room.power_levels",
            "room_id": room_id,
            "sender": "@creator:resident.example.org",
            "state_key": "",
            "content": {
                "users": {"@creator:resident.example.org": 100},
                "users_default": 0,
                "invite": 0, "kick": 50, "ban": 50, "redact": 50, "state_default": 50,
                "events_default": 0, "events": {}, "notifications": {"room": 50},
            },
        });
        let join_rules = serde_json::json!({
            "event_id": "$joinrules",
            "type": "m.room.join_rules",
            "room_id": room_id,
            "sender": "@creator:resident.example.org",
            "state_key": "",
            "content": {"join_rule": "public"},
        });
        let creator_join = serde_json::json!({
            "event_id": "$creatorjoin",
            "type": "m.room.member",
            "room_id": room_id,
            "sender": "@creator:resident.example.org",
            "state_key": "@creator:resident.example.org",
            "content": {"membership": "join"},
        });
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(
            &room_id,
            FakeRoom {
                room_version: Some("11".to_owned()),
                extremities: vec![("$creatorjoin".to_owned(), 4)],
                state: vec![
                    create.clone(),
                    power_levels.clone(),
                    join_rules.clone(),
                    creator_join.clone(),
                ],
                join_auth_chain: vec![create, power_levels, join_rules],
                joined_servers,
                ..FakeRoom::default()
            },
        );
        (rooms, room_id, "11".to_owned())
    }

    /// Stores everything: what `hs-cli`'s real sink does for a valid, new join.
    struct StoringSink;
    #[async_trait]
    impl RoomWriteSink for StoringSink {
        async fn accept_verified_event(
            &self,
            _room_id: &str,
            _event_id: &str,
            _event_json: &Value,
        ) -> Result<WriteOutcome, crate::inbound::WriteRejected> {
            Ok(WriteOutcome::Stored)
        }
    }

    /// Records every `enqueue_pdu` call it receives.
    #[derive(Default)]
    struct RecordingSink(std::sync::Mutex<Vec<(Vec<String>, Value)>>);
    impl OutboundPduSink for RecordingSink {
        fn enqueue_pdu(&self, destinations: Vec<String>, pdu: Value) {
            self.0.lock().unwrap().push((destinations, pdu));
        }
    }

    /// Bob's signed join against [`room_with_creator`]'s state, and its event ID.
    fn bobs_signed_join(keys: &OwnSigningKeys, room_id: &str) -> (Value, String) {
        let signed = sign_member_event(
            keys,
            room_id,
            "@bob:joiner.example.org",
            vec![Value::String("$creatorjoin".to_owned())],
            vec![
                Value::String("$create".to_owned()),
                Value::String("$power".to_owned()),
                Value::String("$joinrules".to_owned()),
            ],
            5,
        );
        let event_id = hs_model::Event::parse(&signed, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();
        (signed, event_id)
    }

    /// The spec's "send the new join event to all other servers in the room": a newly stored
    /// join goes to every member server except the one that submitted it and this one.
    #[tokio::test]
    async fn an_accepted_join_is_forwarded_to_the_other_member_servers_but_not_the_origin() {
        let (rooms, room_id, _) = room_with_creator_and_servers(vec![
            "resident.example.org".to_owned(),
            "other.example.org".to_owned(),
            "third.example.org".to_owned(),
            "joiner.example.org".to_owned(),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let doc = build_server_key_response("joiner.example.org", &keys, &[], 3600).unwrap();
        let cache = RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>);
        let (signed, event_id) = bobs_signed_join(&keys, &room_id);
        let forward = RecordingSink::default();

        let result = send_join(
            &rooms,
            &StoringSink,
            &cache,
            &room_id,
            &event_id,
            &signed,
            "joiner.example.org",
            "resident.example.org",
            Some(&forward),
        )
        .await
        .unwrap();

        let forwarded = forward.0.lock().unwrap().clone();
        assert_eq!(forwarded.len(), 1, "{forwarded:?}");
        let (destinations, pdu) = &forwarded[0];
        assert_eq!(
            destinations,
            &vec![
                "other.example.org".to_owned(),
                "third.example.org".to_owned()
            ]
        );
        assert_eq!(pdu, &result.event);
        assert_eq!(pdu["sender"], "@bob:joiner.example.org");
    }

    /// A join the room already held (a retried `send_join`) was forwarded the first time; the
    /// replay must not fan it out again.
    #[tokio::test]
    async fn a_replayed_join_is_not_forwarded_again() {
        let (rooms, room_id, _) = room_with_creator_and_servers(vec![
            "resident.example.org".to_owned(),
            "other.example.org".to_owned(),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let doc = build_server_key_response("joiner.example.org", &keys, &[], 3600).unwrap();
        let cache = RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>);
        let (signed, event_id) = bobs_signed_join(&keys, &room_id);
        let forward = RecordingSink::default();

        let sink = StaticWriteSink::new(vec![event_id.clone()], "unused");
        send_join(
            &rooms,
            &sink,
            &cache,
            &room_id,
            &event_id,
            &signed,
            "joiner.example.org",
            "resident.example.org",
            Some(&forward),
        )
        .await
        .unwrap();

        assert!(forward.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn make_join_builds_a_real_template_against_current_state() {
        let (rooms, room_id, room_version) = room_with_creator();
        let template = make_join(&rooms, &room_id, "@bob:remote.example.org", &[])
            .await
            .unwrap();
        assert_eq!(template.room_version, room_version);
        assert_eq!(template.event["type"], "m.room.member");
        assert_eq!(template.event["state_key"], "@bob:remote.example.org");
        assert_eq!(template.event["content"]["membership"], "join");
        assert_eq!(
            template.event["prev_events"],
            serde_json::json!(["$creatorjoin"])
        );
        assert_eq!(template.event["depth"], 5);
        let auth_events = template.event["auth_events"].as_array().unwrap();
        assert!(auth_events.contains(&serde_json::json!("$create")));
        assert!(auth_events.contains(&serde_json::json!("$power")));
        assert!(auth_events.contains(&serde_json::json!("$joinrules")));
    }

    #[tokio::test]
    async fn make_join_rejects_an_incompatible_room_version() {
        let (rooms, room_id, _) = room_with_creator();
        let err = make_join(
            &rooms,
            &room_id,
            "@bob:remote.example.org",
            &["9".to_owned()],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::IncompatibleRoomVersion { .. }));
    }

    #[tokio::test]
    async fn make_join_unknown_room_is_room_not_found() {
        let rooms = InMemoryRoomSource::new();
        let err = make_join(&rooms, "!nope:x", "@bob:remote.example.org", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, JoinError::RoomNotFound));
    }

    fn sign_member_event(
        keys: &OwnSigningKeys,
        room_id: &str,
        sender: &str,
        prev_events: Vec<Value>,
        auth_events: Vec<Value>,
        depth: i64,
    ) -> Value {
        let mut object = to_canonical_object(
            &serde_json::json!({
                "type": "m.room.member",
                "room_id": room_id,
                "sender": sender,
                "state_key": sender,
                "origin_server_ts": 1,
                "depth": depth,
                "content": {"membership": "join"},
                "prev_events": prev_events,
                "auth_events": auth_events,
            }),
            true,
        )
        .unwrap();
        let hash = hs_model::hash::content_hash_base64(&object);
        object.insert(
            "hashes".to_owned(),
            CanonicalJsonValue::Object(
                [("sha256".to_owned(), CanonicalJsonValue::String(hash))]
                    .into_iter()
                    .collect(),
            ),
        );
        let server = ruma::ServerName::parse(sender.split_once(':').unwrap().1).unwrap();
        sign_object(&mut object, &server, keys.primary()).unwrap();
        serde_json::from_slice(&CanonicalJsonValue::Object(object).to_canonical_bytes()).unwrap()
    }

    #[tokio::test]
    async fn send_join_validates_and_reports_the_persistence_gap_for_a_new_event() {
        let (rooms, room_id, _) = room_with_creator();
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let doc = build_server_key_response("joiner.example.org", &keys, &[], 3600).unwrap();
        let cache = RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>);

        let signed = sign_member_event(
            &keys,
            &room_id,
            "@bob:joiner.example.org",
            vec![Value::String("$creatorjoin".to_owned())],
            vec![
                Value::String("$create".to_owned()),
                Value::String("$power".to_owned()),
                Value::String("$joinrules".to_owned()),
            ],
            5,
        );
        let event_id = hs_model::Event::parse(&signed, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();

        let sink = StaticWriteSink::new(Vec::new(), "cannot yet persist a newly received event");
        let err = send_join(
            &rooms,
            &sink,
            &cache,
            &room_id,
            &event_id,
            &signed,
            "joiner.example.org",
            "resident.example.org",
            None,
        )
        .await
        .unwrap_err();
        // Real validation succeeded (signature, hash, shape, authorization); only persistence is
        // the honest remaining gap.
        assert!(
            matches!(err, JoinError::Store(_)),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn send_join_rejects_a_join_from_the_wrong_server() {
        let (rooms, room_id, _) = room_with_creator();
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let doc = build_server_key_response("joiner.example.org", &keys, &[], 3600).unwrap();
        let cache = RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>);

        let signed = sign_member_event(
            &keys,
            &room_id,
            "@bob:joiner.example.org",
            vec![Value::String("$creatorjoin".to_owned())],
            vec![],
            5,
        );
        let event_id = hs_model::Event::parse(&signed, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();

        let sink = StaticWriteSink::new(Vec::new(), "unused");
        // Claim the event came from a *different* server than the one that actually signed it.
        let err = send_join(
            &rooms,
            &sink,
            &cache,
            &room_id,
            &event_id,
            &signed,
            "impersonator.example.org",
            "resident.example.org",
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::SenderServerMismatch { .. }));
    }

    #[tokio::test]
    async fn send_join_is_idempotent_for_an_already_known_event() {
        let (rooms, room_id, _) = room_with_creator();
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let doc = build_server_key_response("joiner.example.org", &keys, &[], 3600).unwrap();
        let cache = RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>);

        let signed = sign_member_event(
            &keys,
            &room_id,
            "@bob:joiner.example.org",
            vec![Value::String("$creatorjoin".to_owned())],
            vec![
                Value::String("$create".to_owned()),
                Value::String("$power".to_owned()),
                Value::String("$joinrules".to_owned()),
            ],
            5,
        );
        let event_id = hs_model::Event::parse(&signed, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();

        let sink = StaticWriteSink::new(vec![event_id.clone()], "unused");
        let result = send_join(
            &rooms,
            &sink,
            &cache,
            &room_id,
            &event_id,
            &signed,
            "joiner.example.org",
            "resident.example.org",
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.state.len(), 4);
        assert_eq!(result.auth_chain.len(), 3);
        assert!(!result.members_omitted);
    }
}
