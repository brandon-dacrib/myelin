//! `make_join`/`send_join`: the join handshake for a room this server hosts, called by the server
//! of a user who wants to join it -- and its two siblings, `make_leave`/`send_leave` and
//! `make_knock`/`send_knock`, which are the same handshake with a different `membership`
//! ([`Handshake`]). A leave comes this way when the leaving server is not in the room (a user
//! rejecting an invite, or withdrawing a knock, on a server that holds no copy of the room it
//! could author the leave against); a knock always does. Everything below about `make_join` and
//! `send_join` holds for all three, except that only a join's response carries the room's state.
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
//! - **Room versions 1 and 2** (`EventsReferenceFormat::V1WithHash`) cite each prev and auth
//!   event with its reference hash, so `make_join` reads each cited event's body
//!   ([`crate::room_source::RoomDataSource::event_for_reference`]) to compute it. Until
//!   2026-10-01 they were refused here (`M_UNSUPPORTED_ROOM_VERSION`), and no server could join
//!   this server's version-1 or -2 rooms (Sytest's room-version join tests).

use std::collections::HashMap;

use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use hs_model::room_version::EventsReferenceFormat;
use hs_model::signing::SigningKeyPair;
use hs_state::auth::{self, IncomingEvent};
use hs_state::state_fetch::{StateEntry, StateFetch};
use ruma::{OwnedUserId, RoomId, RoomVersionId, UserId};
use serde_json::Value;

use crate::inbound::{RoomWriteSink, WriteOutcome, event_json, verify_pdu_to_authorise};
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
    /// A restricted join this server cannot vouch for: it is in none of the rooms the join
    /// rules allow, or none of its users there may invite. The spec's
    /// `400 M_UNABLE_TO_AUTHORISE_JOIN`, which tells the joining server to ask another resident.
    UnableToAuthorise(String),
    /// The room is known here but no user of this server is joined to it any more: this server
    /// cannot vouch for the room's current state and will never hear of the events that follow
    /// a join it sponsored. `404 M_NOT_FOUND`, as Synapse answers `make_join` for a room it has
    /// left ("Not an active room on this server").
    NotInRoom,
    Store(String),
    /// In a cluster, the room's shard moved to another replica while the request ran here, and
    /// nothing was stored ([`crate::inbound::WriteRejected::not_owner`]). `503
    /// M_HS_NOT_SHARD_OWNER`: the replica that took the request sends it on to the new owner
    /// (`hs-cli`'s shard gate), and a sender that does get it retries.
    NotOwner(String),
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
            Self::NotAuthorized(e) => write!(f, "not authorized: {e}"),
            Self::UnableToAuthorise(e) => write!(f, "cannot authorise the join: {e}"),
            Self::NotInRoom => write!(f, "not an active room on this server"),
            Self::Store(e) => write!(f, "{e}"),
            Self::NotOwner(e) => write!(f, "this replica no longer owns the room: {e}"),
        }
    }
}

/// Which membership handshake a `make_*`/`send_*` pair is: the three the server-server API
/// defines, each an `m.room.member` event the remote server signs for its own user after the
/// resident hands it a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handshake {
    /// `make_join`/`send_join`.
    Join,
    /// `make_leave`/`send_leave`.
    Leave,
    /// `make_knock`/`send_knock`.
    Knock,
}

impl Handshake {
    /// The `membership` value the handshake's event carries.
    #[must_use]
    pub fn membership(self) -> &'static str {
        match self {
            Self::Join => "join",
            Self::Leave => "leave",
            Self::Knock => "knock",
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

/// One entry in `prev_events`/`auth_events`, encoded per the room version's reference format:
/// the bare event ID (room version 3 and up), or `[event_id, {"sha256": reference hash}]`
/// (versions 1 and 2), for which the referenced event's body is read through
/// [`RoomDataSource::event_for_reference`].
async fn encode_ref(
    rooms: &dyn RoomDataSource,
    room_id: &str,
    event_id: &str,
    room_version: &RoomVersionId,
    rules: &hs_model::room_version::RoomVersionRules,
) -> Result<Value, JoinError> {
    match rules.events_reference_format {
        EventsReferenceFormat::V2IdOnly => Ok(Value::String(event_id.to_owned())),
        EventsReferenceFormat::V1WithHash => {
            let body = rooms
                .event_for_reference(room_id, event_id)
                .await
                .ok_or_else(|| {
                    JoinError::Store(format!("the cited event {event_id} is not held"))
                })?;
            let event = hs_model::Event::parse(&body, room_version.clone()).map_err(|e| {
                JoinError::Store(format!("the cited event {event_id} does not parse: {e}"))
            })?;
            let hash = event.reference_hash().map_err(|e| {
                JoinError::Store(format!("the cited event {event_id} cannot be hashed: {e}"))
            })?;
            Ok(serde_json::json!([
                event_id,
                { "sha256": hs_model::hash::encode_reference_hash(&hash, rules) }
            ]))
        }
    }
}

/// Authorizes a membership event `user` would send about themself with `content`, against
/// `flat` (current state), the way [`auth::check_event_auth`] would authorize the real one.
fn authorize_template(
    rules: &hs_model::room_version::RoomVersionRules,
    flat: &FlatState,
    user: &UserId,
    room_id: &RoomId,
    content: &Value,
    prev_event_count: usize,
) -> Result<(), JoinError> {
    let content = to_canonical_object(content, rules.strict_canonical_json)
        .map_err(|e| JoinError::MalformedEvent(e.to_string()))?;
    let incoming = IncomingEvent {
        event_type: "m.room.member",
        sender: user,
        room_id: Some(room_id),
        state_key: Some(user.as_str()),
        content: &content,
        prev_event_count,
        only_prev_event_is_room_create: false,
        event_id: None,
        redacts: None,
    };
    auth::check_event_auth(rules, &incoming, flat)
        .map_err(|e| JoinError::NotAuthorized(e.to_string()))
}

/// The rooms a restricted room's join rules allow joining from (`allow` entries of type
/// `m.room_membership`), if the room's current join rule is `restricted` (room version 8 and
/// up) or `knock_restricted` (10 and up). `None` for any other join rule.
fn restricted_allow_list(
    rules: &hs_model::room_version::RoomVersionRules,
    flat: &FlatState,
) -> Option<Vec<String>> {
    let entry = flat.get("m.room.join_rules", "")?;
    let rule = entry
        .content
        .get("join_rule")
        .and_then(CanonicalJsonValue::as_str);
    let restricted = (rules.restricted_join_rule && rule == Some("restricted"))
        || (rules.knock_restricted_join_rule && rule == Some("knock_restricted"));
    if !restricted {
        return None;
    }
    let allowed = entry
        .content
        .get("allow")
        .and_then(|allow| match allow {
            CanonicalJsonValue::Array(entries) => Some(entries),
            _ => None,
        })
        .map(|entries| {
            entries
                .iter()
                .filter_map(CanonicalJsonValue::as_object)
                .filter(|entry| {
                    entry.get("type").and_then(CanonicalJsonValue::as_str)
                        == Some("m.room_membership")
                })
                .filter_map(|entry| entry.get("room_id").and_then(CanonicalJsonValue::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(allowed)
}

/// Whether `user` is joined to one of `allowed`, as far as this server can tell: only the rooms
/// it has a joined member in count, since those are the only ones whose membership it knows.
///
/// The error follows Synapse's `check_restricted_join_rules`: when some allowed room is one this
/// server is not in, another server might know `user` is joined there, so the answer is
/// `M_UNABLE_TO_AUTHORISE_JOIN` and the joining server asks another; when this server is in
/// every allowed room (or the join rules allow none at all), nobody can vouch for the join,
/// and it is refused.
///
/// # Errors
/// [`JoinError::UnableToAuthorise`] if `user` is joined to none of the allowed rooms this server
/// is in and it is not in some other allowed room; [`JoinError::NotAuthorized`] if it is in every
/// allowed room (or there are none) and `user` is joined to none of them.
async fn check_allow_list(
    rooms: &dyn RoomDataSource,
    own_server_name: &str,
    allowed: &[String],
    user: &UserId,
) -> Result<(), JoinError> {
    let mut missing_any = false;
    for room in allowed {
        if !rooms
            .member_servers(room)
            .await
            .iter()
            .any(|server| server == own_server_name)
        {
            missing_any = true;
            continue;
        }
        if rooms.membership_of(room, user.as_str()).await.as_deref() == Some("join") {
            return Ok(());
        }
    }
    if missing_any {
        Err(JoinError::UnableToAuthorise(
            "this server is not in every room the join rules allow, and the user is joined to \
             none of those it is in"
                .to_owned(),
        ))
    } else {
        Err(JoinError::NotAuthorized(format!(
            "{user} is not joined to any room the join rules allow"
        )))
    }
}

/// This server's users joined to the room, in a stable order: the candidates to authorise a
/// restricted join.
fn local_members(flat: &FlatState, own_server_name: &str) -> Vec<OwnedUserId> {
    let mut members: Vec<OwnedUserId> = flat
        .by_key
        .iter()
        .filter(|((event_type, _), (_, content, _))| {
            event_type == "m.room.member"
                && content
                    .get("membership")
                    .and_then(CanonicalJsonValue::as_str)
                    == Some("join")
        })
        .filter_map(|((_, state_key), _)| UserId::parse(state_key.as_str()).ok())
        .filter(|user| user.server_name().as_str() == own_server_name)
        .collect();
    members.sort();
    members
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

/// Whether `user_id` is a user of `origin`, the server asking for a membership template for them:
/// a server may only ask on behalf of its own users, since only it can sign their membership
/// event. Compares the whole server name, port included.
///
/// # Errors
/// [`JoinError::SenderServerMismatch`] (`403 M_FORBIDDEN`) when the user is on another server,
/// [`JoinError::MalformedUserId`] when `user_id` is not a user ID at all.
pub fn check_user_is_from_origin(user_id: &str, origin: &str) -> Result<(), JoinError> {
    let user = UserId::parse(user_id).map_err(|e| JoinError::MalformedUserId(e.to_string()))?;
    if user.server_name().as_str() == origin {
        Ok(())
    } else {
        Err(JoinError::SenderServerMismatch {
            sender_server: user.server_name().to_string(),
            origin: origin.to_owned(),
        })
    }
}

/// Builds an unsigned join event template for `user_id` to join `room_id`, against this server's
/// current state. `supported_versions` is the requester's `?ver=` list (empty means "no
/// preference stated", which every version satisfies, matching the spec's default).
///
/// A join to a **restricted** room (`restricted`, room version 8+; `knock_restricted`, 10+) by
/// a user who is neither invited nor joined is authorised here when this server can vouch for
/// it: the user is joined to one of the rooms the join rules allow (as this server sees it), and
/// one of this server's users in the room may invite. The template then names that user in
/// `join_authorised_via_users_server`, and [`send_join`] co-signs the join. A server in none of
/// the allowed rooms answers [`JoinError::UnableToAuthorise`] so the joiner asks another.
///
/// # Errors
/// See [`JoinError`].
pub async fn make_join(
    rooms: &dyn RoomDataSource,
    room_id: &str,
    user_id: &str,
    supported_versions: &[String],
    own_server_name: &str,
) -> Result<JoinTemplate, JoinError> {
    make_membership(
        rooms,
        room_id,
        user_id,
        supported_versions,
        Handshake::Join,
        own_server_name,
    )
    .await
}

/// [`make_join`] for any [`Handshake`]: an unsigned `m.room.member` template with `handshake`'s
/// membership, checked against current state the same way. A knock is refused up front in a room
/// version without knocking; a leave is authorized like any other (a user who is not in the
/// room, not invited and not knocking has nothing to leave).
///
/// # Errors
/// See [`JoinError`].
pub async fn make_membership(
    rooms: &dyn RoomDataSource,
    room_id: &str,
    user_id: &str,
    supported_versions: &[String],
    handshake: Handshake,
    own_server_name: &str,
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
    // A room version without knocking is the room refusing knocks, not a version the knocking
    // server lacks: `403 M_FORBIDDEN`, as the spec's `make_knock` describes a room "configured to
    // prevent knocks" and as Synapse answers.
    if handshake == Handshake::Knock && !rules.knocking {
        return Err(JoinError::NotAuthorized(format!(
            "room version {room_version_str} does not support knocking"
        )));
    }

    let user = UserId::parse(user_id).map_err(|e| JoinError::MalformedUserId(e.to_string()))?;
    let parsed_room_id = RoomId::parse(room_id).map_err(|_| JoinError::RoomNotFound)?;

    // A room everybody here has left is still known (its events are kept), but this server is no
    // longer a resident: its state may be stale and it would never hear of what follows the
    // join. Synapse answers `404 M_NOT_FOUND` ("Not an active room on this server"), as Sytest's
    // "Inbound /make_join rejects attempts to join rooms where all users have left" expects; a
    // template was handed out until 2026-10-01.
    if handshake == Handshake::Join
        && !rooms
            .member_servers(room_id)
            .await
            .iter()
            .any(|server| server == own_server_name)
    {
        tracing::info!(%room_id, %user_id, "refused a make_join for a room no user of this server is in");
        return Err(JoinError::NotInRoom);
    }

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
    let mut prev_events = Vec::with_capacity(extremities.len());
    for (id, _) in &extremities {
        prev_events.push(encode_ref(rooms, room_id, id, &room_version, &rules).await?);
    }

    let state = rooms.state_for_join(room_id).await.map_err(source_err)?;
    let flat = FlatState::build(&state.state);

    let mut content = serde_json::json!({ "membership": handshake.membership() });
    // A best-effort early check: authorizing the template we are about to hand out against
    // *current* state, so a request that is hopeless (banned user, invite-only room with no
    // invite) gets a clear rejection now rather than a template the eventual `send_join` will
    // reject anyway. This is not a substitute for `send_join`'s own check -- state can move
    // between the two calls -- it is a courtesy.
    let authorized = authorize_template(
        &rules,
        &flat,
        &user,
        &parsed_room_id,
        &content,
        extremities.len(),
    );
    if let Err(refusal) = authorized {
        // A restricted room the user may join without an invite, if a member of one of the
        // rooms its join rules allow: this server vouches for that by naming one of its own
        // users who may invite (`join_authorised_via_users_server`), and co-signs the join at
        // `send_join`. Whether the user is in an allowed room is this server's view of it.
        let allowed = match handshake {
            Handshake::Join => restricted_allow_list(&rules, &flat),
            Handshake::Leave | Handshake::Knock => None,
        };
        let Some(allowed) = allowed else {
            return Err(refusal);
        };
        check_allow_list(rooms, own_server_name, &allowed, &user).await?;
        let authoriser = local_members(&flat, own_server_name)
            .into_iter()
            .find(|candidate| {
                let mut with = content.clone();
                with["join_authorised_via_users_server"] = Value::String(candidate.to_string());
                authorize_template(
                    &rules,
                    &flat,
                    &user,
                    &parsed_room_id,
                    &with,
                    extremities.len(),
                )
                .is_ok()
            })
            .ok_or_else(|| {
                JoinError::UnableToAuthorise(
                    "no user of this server in the room may invite".to_owned(),
                )
            })?;
        content["join_authorised_via_users_server"] = Value::String(authoriser.to_string());
    }

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
            auth_events.push(encode_ref(rooms, room_id, id, &room_version, &rules).await?);
        }
    }

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
    authorise_with: Option<&SigningKeyPair>,
) -> Result<SendJoinResult, JoinError> {
    send_membership(
        rooms,
        sink,
        key_cache,
        room_id,
        event_id,
        signed_event,
        origin,
        own_server_name,
        forward,
        Handshake::Join,
        authorise_with,
    )
    .await
}

/// [`send_join`] for any [`Handshake`]: the submitted event must carry `handshake`'s membership
/// and be about its own sender, and is verified, authorized, stored and forwarded to the room's
/// other servers exactly as a join is. The result carries the room's state for every handshake;
/// `send_leave` answers with none of it and `send_knock` with its stripped form
/// (`crate::stripped`), which is the transport's business.
///
/// A restricted join naming one of this server's users in `join_authorised_via_users_server`
/// (the template [`make_join`] handed out, or one the joining server made up) is checked the way
/// `make_join` checks it -- the user is joined to a room the join rules allow, as this server
/// sees it -- and co-signed with `authorise_with`, this server's event-signing key; the
/// co-signed event is what is stored, forwarded and answered with. Without a key such a join
/// needs this server's signature already, which it will not have.
///
/// # Errors
/// See [`JoinError`].
#[allow(clippy::too_many_arguments)]
pub async fn send_membership(
    rooms: &dyn RoomDataSource,
    sink: &dyn RoomWriteSink,
    key_cache: &DynRemoteKeyCache,
    room_id: &str,
    event_id: &str,
    signed_event: &Value,
    origin: &str,
    own_server_name: &str,
    forward: Option<&dyn OutboundPduSink>,
    handshake: Handshake,
    authorise_with: Option<&SigningKeyPair>,
) -> Result<SendJoinResult, JoinError> {
    let Some(room_version_str) = rooms.room_version(room_id).await else {
        return Err(JoinError::RoomNotFound);
    };
    let room_version = RoomVersionId::try_from(room_version_str.as_str())
        .map_err(|_| JoinError::UnsupportedRoomVersion(room_version_str.clone()))?;
    let rules = hs_model::room_version::rules_for(&room_version)
        .ok_or_else(|| JoinError::UnsupportedRoomVersion(room_version_str.clone()))?;

    // An event the room version's canonical JSON cannot carry (a float, an integer out of range
    // in version 6 and later) is a bad request before its signature is looked at: the signature
    // check canonicalises too, and would call it unsigned (Sytest's "Inbound: send_join rejects
    // invalid JSON for room version 6").
    to_canonical_object(signed_event, rules.strict_canonical_json)
        .map_err(|e| JoinError::MalformedEvent(format!("not canonical JSON: {e}")))?;

    let authorising = authorise_with.map(|_| own_server_name);
    // A membership event not signed as it must be is the room refusing it (`403 M_FORBIDDEN`,
    // as Synapse answers and Sytest's "Inbound /v1/send_join rejects incorrectly-signed joins"
    // expects); one that does not parse is a bad request. Both were `400 M_BAD_JSON` until
    // 2026-10-01.
    let mut event = verify_pdu_to_authorise(signed_event, &room_version, key_cache, authorising)
        .await
        .map_err(|e| {
            if e.unsigned {
                JoinError::NotAuthorized(e.to_string())
            } else {
                JoinError::MalformedEvent(e.to_string())
            }
        })?;

    // Whose event it is comes first: a server submits its own users' membership events only, and
    // one replaying another server's is refused (`403`) before anything else is said about it
    // (Sytest's "Inbound /v1/send_join rejects joins from other servers").
    let sender_server = event.header().sender.server_name().as_str().to_owned();
    if sender_server != origin {
        return Err(JoinError::SenderServerMismatch {
            sender_server: sender_server.to_owned(),
            origin: origin.to_owned(),
        });
    }
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
    if membership != Some(handshake.membership()) {
        return Err(JoinError::MalformedEvent(format!(
            "content.membership is not \"{}\"",
            handshake.membership()
        )));
    }
    if event.header().state_key.as_deref() != Some(event.header().sender.as_str()) {
        return Err(JoinError::MalformedEvent(
            "a membership handshake's event must be about its own sender".to_owned(),
        ));
    }
    let event_room_id = event
        .json()
        .get("room_id")
        .and_then(CanonicalJsonValue::as_str);
    if event_room_id != Some(room_id) {
        return Err(JoinError::RoomIdMismatch);
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

    let authoriser = crate::inbound::join_authoriser_server(&event);
    if let (Some(key), Some(authoriser)) = (authorise_with, authoriser.as_deref())
        && handshake == Handshake::Join
        && authoriser == own_server_name
        && authoriser != sender_server
    {
        // The auth check above has established that the named user is joined and may invite;
        // what it cannot know is whether the joiner may join through the allow list, which is
        // this server's to vouch for. An invited (or already joined) user needs no vouching.
        let sender: &UserId = event.header().sender.as_ref();
        let current = flat
            .get("m.room.member", sender.as_str())
            .and_then(|entry| {
                entry
                    .content
                    .get("membership")
                    .and_then(CanonicalJsonValue::as_str)
            })
            .map(str::to_owned);
        if !matches!(current.as_deref(), Some("join" | "invite"))
            && let Some(allowed) = restricted_allow_list(&rules, &flat)
        {
            check_allow_list(rooms, own_server_name, &allowed, sender).await?;
        }
        let own = ruma::ServerName::parse(own_server_name)
            .map_err(|e| JoinError::Store(format!("this server's name: {e}")))?;
        let cosigned = crate::invite::cosign(&event, &own, key)
            .map_err(|e| JoinError::Store(e.to_string()))?;
        event = hs_model::Event::parse(&cosigned, room_version.clone())
            .map_err(|e| JoinError::Store(format!("the co-signed join does not parse: {e}")))?;
        tracing::info!(room_id, event_id, "authorised a restricted join");
    }

    let value = event_json(&event);
    let outcome = sink
        .accept_verified_event(room_id, event.event_id().as_str(), &value)
        .await
        .map_err(|e| {
            if e.not_owner {
                JoinError::NotOwner(e.error)
            } else {
                JoinError::Store(e.error)
            }
        })?;
    tracing::info!(
        room_id,
        event_id,
        ?outcome,
        membership = handshake.membership(),
        "membership handshake: event accepted"
    );

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
                "membership handshake: forwarding the accepted event to the room's other servers"
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
        room_fixture("public", Vec::new(), joined_servers)
    }

    /// The same room with `join_rule` and `extra_state` (member events, say) added to its state.
    fn room_fixture(
        join_rule: &str,
        extra_state: Vec<Value>,
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
            "content": {"join_rule": join_rule},
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
                state: [
                    create.clone(),
                    power_levels.clone(),
                    join_rules.clone(),
                    creator_join.clone(),
                ]
                .into_iter()
                .chain(extra_state)
                .collect(),
                join_auth_chain: vec![create, power_levels, join_rules],
                joined_servers,
                ..FakeRoom::default()
            },
        );
        (rooms, room_id, "11".to_owned())
    }

    /// A version-1 room: `make_join` cites the extremity and each auth event as
    /// `[event_id, {"sha256": reference hash}]`, the hash computed from the cited event's body.
    /// Until 2026-10-01 it answered `M_UNSUPPORTED_ROOM_VERSION` instead.
    #[tokio::test]
    async fn make_join_cites_events_by_reference_hash_in_a_version_1_room() {
        let (fixture, room_id, _) = room_with_creator();
        let (_, mut room) = fixture.rooms_for_test().into_iter().next().unwrap();
        let mut depth = 0;
        for event in &mut room.state {
            depth += 1;
            let id = event["event_id"].as_str().unwrap().to_owned();
            event["event_id"] = Value::String(format!("{id}:resident.example.org"));
            event["origin"] = Value::String("resident.example.org".to_owned());
            event["origin_server_ts"] = serde_json::json!(1);
            event["depth"] = serde_json::json!(depth);
            event["prev_events"] = serde_json::json!([]);
            event["auth_events"] = serde_json::json!([]);
            event["hashes"] = serde_json::json!({"sha256": "aGFzaA"});
            event["signatures"] = serde_json::json!({});
            if event["type"] == "m.room.create" {
                event["content"]["room_version"] = Value::String("1".to_owned());
            }
        }
        room.room_version = Some("1".to_owned());
        room.extremities = vec![("$creatorjoin:resident.example.org".to_owned(), depth)];
        let state = room.state.clone();
        let mut rooms = InMemoryRoomSource::new();
        rooms.insert_room(&room_id, room);

        let template = make_join(
            &rooms,
            &room_id,
            "@bob:joiner.example.org",
            &["1".to_owned()],
            "resident.example.org",
        )
        .await
        .unwrap();
        let expected_ref = |id: &str| {
            let body = state.iter().find(|e| e["event_id"] == id).unwrap();
            let event = hs_model::Event::parse(body, RoomVersionId::V1).unwrap();
            let rules = hs_model::room_version::rules_for(&RoomVersionId::V1).unwrap();
            serde_json::json!([
                id,
                {"sha256": hs_model::hash::encode_reference_hash(
                    &event.reference_hash().unwrap(), &rules)}
            ])
        };
        assert_eq!(
            template.event["prev_events"],
            serde_json::json!([expected_ref("$creatorjoin:resident.example.org")])
        );
        let auth = template.event["auth_events"].as_array().unwrap();
        assert!(auth.contains(&expected_ref("$create:resident.example.org")));
        assert!(auth.contains(&expected_ref("$power:resident.example.org")));
        assert_eq!(template.room_version, "1");
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
            None,
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
            None,
        )
        .await
        .unwrap();

        assert!(forward.0.lock().unwrap().is_empty());
    }

    fn bobs_invite() -> Value {
        serde_json::json!({
            "event_id": "$bobinvite",
            "type": "m.room.member",
            "room_id": "!r:resident.example.org",
            "sender": "@creator:resident.example.org",
            "state_key": "@bob:joiner.example.org",
            "content": {"membership": "invite"},
        })
    }

    #[tokio::test]
    async fn make_leave_hands_an_invited_user_a_leave_template_and_a_stranger_nothing() {
        let (rooms, room_id, _) = room_fixture("invite", vec![bobs_invite()], Vec::new());
        let template = make_membership(
            &rooms,
            &room_id,
            "@bob:joiner.example.org",
            &[],
            Handshake::Leave,
            "resident.example.org",
        )
        .await
        .unwrap();
        assert_eq!(template.event["content"]["membership"], "leave");
        assert_eq!(template.event["state_key"], "@bob:joiner.example.org");
        let auth_events = template.event["auth_events"].as_array().unwrap();
        assert!(auth_events.contains(&serde_json::json!("$bobinvite")));

        let err = make_membership(
            &rooms,
            &room_id,
            "@stranger:joiner.example.org",
            &[],
            Handshake::Leave,
            "resident.example.org",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::NotAuthorized(_)), "{err}");
    }

    #[tokio::test]
    async fn make_knock_needs_a_knock_room_in_a_version_with_knocking() {
        let (rooms, room_id, _) = room_fixture("knock", Vec::new(), Vec::new());
        let template = make_membership(
            &rooms,
            &room_id,
            "@bob:joiner.example.org",
            &["11".to_owned()],
            Handshake::Knock,
            "resident.example.org",
        )
        .await
        .unwrap();
        assert_eq!(template.event["content"]["membership"], "knock");

        let (public, room_id, _) = room_fixture("public", Vec::new(), Vec::new());
        let err = make_membership(
            &public,
            &room_id,
            "@bob:joiner.example.org",
            &["11".to_owned()],
            Handshake::Knock,
            "resident.example.org",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::NotAuthorized(_)), "{err}");

        // A room version without knocking refuses the knock (`403`), as Synapse does; it is not
        // a version the knocking server lacks (`400 M_INCOMPATIBLE_ROOM_VERSION`).
        let mut old = InMemoryRoomSource::new();
        old.insert_room(
            "!old:resident.example.org",
            FakeRoom {
                room_version: Some("6".to_owned()),
                ..FakeRoom::default()
            },
        );
        let err = make_membership(
            &old,
            "!old:resident.example.org",
            "@bob:joiner.example.org",
            &["6".to_owned(), "11".to_owned()],
            Handshake::Knock,
            "resident.example.org",
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, JoinError::NotAuthorized(msg) if msg.contains("does not support knocking")),
            "{err}"
        );
    }

    /// A knock the resident accepts is stored and forwarded to the room's other servers, as a
    /// join is; one whose membership is not "knock" is refused before anything is stored.
    #[tokio::test]
    async fn send_knock_stores_and_forwards_a_knock_and_refuses_anything_else() {
        let (rooms, room_id, _) = room_fixture(
            "knock",
            Vec::new(),
            vec![
                "resident.example.org".to_owned(),
                "other.example.org".to_owned(),
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let doc = build_server_key_response("joiner.example.org", &keys, &[], 3600).unwrap();
        let cache = RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>);
        let auth = vec![
            Value::String("$create".to_owned()),
            Value::String("$power".to_owned()),
            Value::String("$joinrules".to_owned()),
        ];
        let knock = sign_membership_event(
            &keys,
            &room_id,
            "@bob:joiner.example.org",
            "knock",
            vec![Value::String("$creatorjoin".to_owned())],
            auth.clone(),
            5,
        );
        let knock_id = hs_model::Event::parse(&knock, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();
        let forward = RecordingSink::default();
        let result = send_membership(
            &rooms,
            &StoringSink,
            &cache,
            &room_id,
            &knock_id,
            &knock,
            "joiner.example.org",
            "resident.example.org",
            Some(&forward),
            Handshake::Knock,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.event["content"]["membership"], "knock");
        let forwarded = forward.0.lock().unwrap().clone();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].0, vec!["other.example.org".to_owned()]);

        let join = sign_membership_event(
            &keys,
            &room_id,
            "@bob:joiner.example.org",
            "join",
            vec![Value::String("$creatorjoin".to_owned())],
            auth,
            5,
        );
        let join_id = hs_model::Event::parse(&join, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();
        let err = send_membership(
            &rooms,
            &StoringSink,
            &cache,
            &room_id,
            &join_id,
            &join,
            "joiner.example.org",
            "resident.example.org",
            None,
            Handshake::Knock,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::MalformedEvent(_)), "{err}");
    }

    /// Rejecting an invite from a server that is not in the room: the invited user's server
    /// signs the leave template and the resident stores it.
    #[tokio::test]
    async fn send_leave_accepts_an_invited_users_own_leave() {
        let (rooms, room_id, _) = room_fixture("invite", vec![bobs_invite()], Vec::new());
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let doc = build_server_key_response("joiner.example.org", &keys, &[], 3600).unwrap();
        let cache = RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>);
        let leave = sign_membership_event(
            &keys,
            &room_id,
            "@bob:joiner.example.org",
            "leave",
            vec![Value::String("$creatorjoin".to_owned())],
            vec![
                Value::String("$create".to_owned()),
                Value::String("$power".to_owned()),
                Value::String("$bobinvite".to_owned()),
            ],
            5,
        );
        let leave_id = hs_model::Event::parse(&leave, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string();
        let result = send_membership(
            &rooms,
            &StoringSink,
            &cache,
            &room_id,
            &leave_id,
            &leave,
            "joiner.example.org",
            "resident.example.org",
            None,
            Handshake::Leave,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.event["content"]["membership"], "leave");
    }

    #[tokio::test]
    async fn make_join_builds_a_real_template_against_current_state() {
        let (rooms, room_id, room_version) = room_with_creator();
        let template = make_join(
            &rooms,
            &room_id,
            "@bob:remote.example.org",
            &[],
            "resident.example.org",
        )
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
            "resident.example.org",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::IncompatibleRoomVersion { .. }));
    }

    #[tokio::test]
    async fn make_join_unknown_room_is_room_not_found() {
        let rooms = InMemoryRoomSource::new();
        let err = make_join(
            &rooms,
            "!nope:x",
            "@bob:remote.example.org",
            &[],
            "resident.example.org",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::RoomNotFound));
    }

    /// A restricted room joinable from `!lobby:resident.example.org`, and the lobby: held here
    /// (this server has a member in it) when `lobby_held`, with bob joined to it when
    /// `bob_in_lobby`.
    fn restricted_fixture(lobby_held: bool, bob_in_lobby: bool) -> (InMemoryRoomSource, String) {
        let lobby_id = "!lobby:resident.example.org";
        let allow = serde_json::json!({
            "event_id": "$allow",
            "type": "m.room.join_rules",
            "room_id": "!r:resident.example.org",
            "sender": "@creator:resident.example.org",
            "state_key": "",
            "content": {
                "join_rule": "restricted",
                "allow": [{"type": "m.room_membership", "room_id": lobby_id}],
            },
        });
        let (mut rooms, room_id, _) = room_fixture(
            "restricted",
            vec![allow],
            vec!["resident.example.org".to_owned()],
        );
        let bob_join = serde_json::json!({
            "event_id": "$boblobby",
            "type": "m.room.member",
            "room_id": lobby_id,
            "sender": "@bob:remote.example.org",
            "state_key": "@bob:remote.example.org",
            "content": {"membership": "join"},
        });
        rooms.insert_room(
            lobby_id,
            FakeRoom {
                room_version: Some("11".to_owned()),
                state: if bob_in_lobby {
                    vec![bob_join]
                } else {
                    Vec::new()
                },
                joined_servers: if lobby_held {
                    vec!["resident.example.org".to_owned()]
                } else {
                    Vec::new()
                },
                ..FakeRoom::default()
            },
        );
        (rooms, room_id)
    }

    #[tokio::test]
    async fn make_join_names_a_local_authoriser_for_a_restricted_room() {
        let (rooms, room_id) = restricted_fixture(true, true);
        let template = make_join(
            &rooms,
            &room_id,
            "@bob:remote.example.org",
            &[],
            "resident.example.org",
        )
        .await
        .unwrap();
        assert_eq!(
            template.event["content"]["join_authorised_via_users_server"],
            "@creator:resident.example.org"
        );
        let auth_events = template.event["auth_events"].as_array().unwrap();
        assert!(
            auth_events.contains(&serde_json::json!("$creatorjoin")),
            "the authoriser's membership is an auth event: {auth_events:?}"
        );
        assert!(auth_events.contains(&serde_json::json!("$allow")));
    }

    #[tokio::test]
    async fn make_join_refuses_a_restricted_join_it_cannot_vouch_for() {
        let (rooms, room_id) = restricted_fixture(true, false);
        let err = make_join(
            &rooms,
            &room_id,
            "@bob:remote.example.org",
            &[],
            "resident.example.org",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::NotAuthorized(_)), "{err}");

        let (rooms, room_id) = restricted_fixture(false, true);
        let err = make_join(
            &rooms,
            &room_id,
            "@bob:remote.example.org",
            &[],
            "resident.example.org",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::UnableToAuthorise(_)), "{err}");

        // Join rules that allow no room at all: nobody can vouch for the join (Synapse's 403).
        let allow_none = serde_json::json!({
            "event_id": "$allow",
            "type": "m.room.join_rules",
            "room_id": "!r:resident.example.org",
            "sender": "@creator:resident.example.org",
            "state_key": "",
            "content": {"join_rule": "knock_restricted", "allow": []},
        });
        let (rooms, room_id, _) = room_fixture(
            "knock_restricted",
            vec![allow_none],
            vec!["resident.example.org".to_owned()],
        );
        let err = make_join(
            &rooms,
            &room_id,
            "@bob:remote.example.org",
            &[],
            "resident.example.org",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::NotAuthorized(_)), "{err}");
    }

    fn sign_member_event(
        keys: &OwnSigningKeys,
        room_id: &str,
        sender: &str,
        prev_events: Vec<Value>,
        auth_events: Vec<Value>,
        depth: i64,
    ) -> Value {
        sign_membership_event(
            keys,
            room_id,
            sender,
            "join",
            prev_events,
            auth_events,
            depth,
        )
    }

    fn sign_membership_event(
        keys: &OwnSigningKeys,
        room_id: &str,
        sender: &str,
        membership: &str,
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
                "content": {"membership": membership},
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
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::SenderServerMismatch { .. }));

        // Replayed under a path naming another event: still the server mismatch, said first.
        let err = send_join(
            &rooms,
            &sink,
            &cache,
            &room_id,
            "$some-other-event",
            &signed,
            "impersonator.example.org",
            "resident.example.org",
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, JoinError::SenderServerMismatch { .. }),
            "{err}"
        );
    }

    /// A join that is not signed, or signed with a signature that does not verify, is the room
    /// refusing it -- `403 M_FORBIDDEN` (`JoinError::NotAuthorized`), as Synapse answers and
    /// Sytest's "Inbound /v1/send_join rejects incorrectly-signed joins" expects. It was
    /// `400 M_BAD_JSON` until 2026-10-01.
    #[tokio::test]
    async fn send_join_refuses_an_unsigned_or_badly_signed_join_as_forbidden() {
        let (rooms, room_id, _) = room_with_creator();
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let doc = build_server_key_response("joiner.example.org", &keys, &[], 3600).unwrap();
        let cache = RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>);
        let mut signed = sign_member_event(
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
        for signatures in [
            serde_json::json!({}),
            serde_json::json!({"joiner.example.org": {keys.primary().key_id(): "A".repeat(86)}}),
        ] {
            signed["signatures"] = signatures;
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
                None,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, JoinError::NotAuthorized(_)), "{err}");
        }
    }

    /// Sytest's "Inbound: send_join rejects invalid JSON for room version 6": a float in the
    /// event is a bad request, not a signature failure (`403`), which it was while the
    /// signature was checked first.
    #[tokio::test]
    async fn send_join_refuses_an_event_that_is_not_canonical_json_as_malformed() {
        let (rooms, room_id, _) = room_with_creator();
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let doc = build_server_key_response("joiner.example.org", &keys, &[], 3600).unwrap();
        let cache = RemoteKeyCache::new(Box::new(FixedFetcher(doc)) as Box<dyn KeyServerFetcher>);
        let mut signed = sign_member_event(
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
        signed["content"]["bad_val"] = serde_json::json!(1.1);
        let sink = StaticWriteSink::new(Vec::new(), "unused");
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
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, JoinError::MalformedEvent(_)), "{err}");
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
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.state.len(), 4);
        assert_eq!(result.auth_chain.len(), 3);
        assert!(!result.members_omitted);
    }
}
