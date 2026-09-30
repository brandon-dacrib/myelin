//! The space hierarchy (MSC2946, spec 1.2+): what `GET /_matrix/client/v1/rooms/{roomId}/hierarchy`
//! (`crate::routes::hierarchy`) walks and what the federation `GET /hierarchy/{roomId}` answers
//! with (`hs-cli`'s `RoomDataSource::hierarchy`, over the same helpers).
//!
//! # The pieces
//!
//! - [`child_links`]: a room's `m.space.child` links, in the spec's order (a valid `order` key
//!   first, then `origin_server_ts`), minus the ones with no `via` (which is how a link is
//!   removed) and, when asked, minus the ones not marked `suggested`.
//! - [`summarize`]: one room's [`RoomSummary`], the `PublicRoomsChunk`-plus-`room_type`-and-
//!   `allowed_room_ids` shape both APIs return per room.
//! - [`local_access`] and [`server_access`]: whether a room may be shown to a user of this
//!   server, or to another server, per the spec's list ("the requester may be able to see the
//!   room": joined or invited, public, knockable, world-readable, or restricted to a room the
//!   requester is in). A restricted room's answer needs the requester's other memberships, which
//!   an actor cannot see, so the answer is an [`Access`] the caller resolves.
//! - [`RemoteHierarchy`]: the seam through which a child this server does not hold is asked of
//!   the servers in its `via`, the same way `crate::remote_join` and `crate::backfill` reach
//!   federation: this crate defines the trait, `hs-cli` implements it over the federation client
//!   and installs it on the registry ([`crate::registry::RoomRegistry::install_remote_hierarchy`]).
//! - [`walk`]: the client endpoint itself, depth-first, paginated with an opaque token that
//!   expires ([`PaginationSessions`]).
//!
//! # Order of the walk
//!
//! Depth-first, pre-order, each room's children in [`child_links`]' order: the spec says
//! "paginates over the space tree in a depth-first manner", and Complement's
//! `TestClientSpacesSummary/pagination` pins it (a `limit=4` first page of a root with children
//! `R1, SS1, R2` and `SS1 -> SS2` is `Root, R1, SS1, SS2`; the next page is `R3, R4, R2`).
//! Synapse walks the same way, with the same visit-once rule for a room linked from two places.
//!
//! # What a page costs
//!
//! At most [`MAX_LIMIT`] rooms are summarised per page. A room this server holds costs one actor
//! query. A room it does not hold costs up to [`MAX_SERVERS_PER_CHILD`] federation requests,
//! each bounded by the federation client's own timeout and per-destination backoff; a server
//! that fails is skipped, never fatal. A federation answer's `children` are kept on the queue
//! entries of the children they describe, so a leaf room summarised by its server is not asked
//! for again, and a `inaccessible_children` entry is dropped from the walk without a request.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hs_kv::KvBackend;
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use ruma::{OwnedRoomId, RoomId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::actor::RoomActor;
use crate::error::RoomError;
use crate::registry::RoomRegistry;

/// How many rooms a page holds when the client names no `limit`, and the most it may ask for.
/// Synapse's `MAX_ROOMS`, which is what Element is written against.
pub const MAX_LIMIT: usize = 50;

/// How many of a space's children the federation `/hierarchy` answer summarises, and how many
/// the client walk queues from one room. Synapse's `MAX_ROOMS_PER_SPACE`.
pub const MAX_CHILDREN_PER_SPACE: usize = 50;

/// How many of a child's `via` servers are asked before the child is given up on for this
/// page. Synapse's `MAX_SERVERS_PER_SPACE`.
pub const MAX_SERVERS_PER_CHILD: usize = 3;

/// How long a `next_batch` token stays usable. Synapse's five minutes.
pub const TOKEN_VALIDITY: Duration = Duration::from_secs(5 * 60);

/// The most pagination sessions kept at once; past it, the oldest is dropped. A session is a
/// queue of room IDs and a set of visited ones, so this bounds memory at a few megabytes for
/// a server whose clients page the largest spaces.
const MAX_SESSIONS: usize = 4_096;

/// The longest `order` the spec honours: 50 characters, all in `\x20..=\x7E`.
const MAX_ORDER_LEN: usize = 50;

// ------------------------------------------------------------------------------------------
// Children
// ------------------------------------------------------------------------------------------

/// One `m.space.child` link of a space, as the spec's stripped child state event plus what the
/// walk needs from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLink {
    /// The child room: the event's `state_key`.
    pub room_id: OwnedRoomId,
    /// The servers to ask about the child (`content.via`), never empty.
    pub via: Vec<String>,
    /// `content.suggested`, `false` when absent or not a boolean.
    pub suggested: bool,
    /// `content.order` when it is a valid one (a string of at most 50 characters in the
    /// printable ASCII range), which sorts before any link without one.
    pub order: Option<String>,
    /// The link event's `origin_server_ts`.
    pub origin_server_ts: i64,
    /// The link event's `sender`.
    pub sender: String,
    /// The link event's `content`, as sent.
    pub content: Value,
}

impl ChildLink {
    /// Reads a link out of a current-state event. `None` for anything that is not an
    /// `m.space.child` with a non-empty `via` of strings: a link with no `via` (the way a link
    /// is removed) or a malformed one is not a child.
    #[must_use]
    pub fn from_event(event: &Event) -> Option<Self> {
        let header = event.header();
        if header.event_type != "m.space.child" {
            return None;
        }
        let room_id = RoomId::parse(header.state_key.as_deref()?).ok()?.to_owned();
        let content = canonical_to_value(event.json().get("content")?);
        let via: Vec<String> = content
            .get("via")?
            .as_array()?
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        if via.is_empty() {
            return None;
        }
        let suggested = content
            .get("suggested")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let order = content
            .get("order")
            .and_then(Value::as_str)
            .filter(|order| valid_order(order))
            .map(str::to_owned);
        Some(Self {
            room_id,
            via,
            suggested,
            order,
            origin_server_ts: header.origin_server_ts,
            sender: header.sender.to_string(),
            content,
        })
    }

    /// The link as the spec's `StrippedChildStateEvent`: a stripped state event with
    /// `origin_server_ts` added.
    #[must_use]
    pub fn stripped(&self) -> Value {
        json!({
            "type": "m.space.child",
            "state_key": self.room_id,
            "sender": self.sender,
            "content": self.content,
            "origin_server_ts": self.origin_server_ts,
        })
    }
}

/// Whether `order` is one the spec honours: at most 50 characters, each in `\x20..=\x7E`.
fn valid_order(order: &str) -> bool {
    order.len() <= MAX_ORDER_LEN && order.bytes().all(|b| (0x20..=0x7E).contains(&b))
}

/// Sorts links the way the spec orders a space's children: those with a valid `order` first,
/// by that key (bytewise, which for printable ASCII is by code point), then by the link event's
/// `origin_server_ts`, then by child room ID so the order is total.
pub fn sort_child_links(links: &mut [ChildLink]) {
    links.sort_by(|a, b| {
        match (&a.order, &b.order) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.origin_server_ts.cmp(&b.origin_server_ts))
        .then_with(|| a.room_id.cmp(&b.room_id))
    });
}

/// A room's `m.space.child` links in the spec's order ([`sort_child_links`]), without the ones
/// that are not `suggested` when `suggested_only`. Empty for a room that is not a space: the
/// spec's `children_state` "should be empty" for one, and Complement's `TestClientSpacesSummary`
/// links a plain room to another and expects the link ignored.
///
/// # Errors
/// [`RoomError::State`] if the current state cannot be read.
pub fn child_links<B: KvBackend>(
    actor: &RoomActor<B>,
    suggested_only: bool,
) -> Result<Vec<ChildLink>, RoomError> {
    if actor.creation_type().as_deref() != Some("m.space") {
        return Ok(Vec::new());
    }
    let mut links: Vec<ChildLink> = actor
        .full_state()?
        .into_iter()
        .filter_map(ChildLink::from_event)
        .filter(|link| !suggested_only || link.suggested)
        .collect();
    sort_child_links(&mut links);
    Ok(links)
}

// ------------------------------------------------------------------------------------------
// Summaries
// ------------------------------------------------------------------------------------------

/// One room as both hierarchy APIs describe it: the spec's `RoomSummary` (`PublicRoomsChunk`
/// plus `room_type`, `allowed_room_ids`, `encryption` and `room_version`). Optional fields are
/// left out of the JSON when unset, as `/publicRooms` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSummary {
    /// The room.
    pub room_id: OwnedRoomId,
    /// `m.room.name`'s `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `m.room.topic`'s `topic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// `m.room.canonical_alias`'s `alias`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_alias: Option<String>,
    /// `m.room.avatar`'s `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// How many members hold `join`.
    pub num_joined_members: u64,
    /// `m.room.join_rules`' `join_rule`; absent when the room has no join rules event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_rule: Option<String>,
    /// `m.room.create`'s `type` (`m.space` for a space).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_type: Option<String>,
    /// Whether `m.room.history_visibility` is `world_readable`.
    pub world_readable: bool,
    /// Whether `m.room.guest_access` is `can_join`.
    pub guest_can_join: bool,
    /// For a `restricted` or `knock_restricted` room, the rooms its `allow` list names
    /// (`m.room_membership` entries); left out otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_room_ids: Vec<OwnedRoomId>,
    /// `m.room.encryption`'s `algorithm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<String>,
    /// The room's version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_version: Option<String>,
}

impl RoomSummary {
    /// The summary as JSON, with `children_state` set to `children` (the client endpoint's
    /// `SpaceHierarchyRoomsChunk`, which requires the key even for a room with no children).
    #[must_use]
    pub fn to_json_with_children(&self, children: &[ChildLink]) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        if let Value::Object(map) = &mut value {
            map.insert(
                "children_state".to_owned(),
                Value::Array(children.iter().map(ChildLink::stripped).collect()),
            );
        }
        value
    }
}

/// Reads `content.<key>` of a current-state event as a string.
fn content_str<B: KvBackend>(actor: &RoomActor<B>, event_type: &str, key: &str) -> Option<String> {
    actor
        .state_event(event_type, "")
        .ok()
        .flatten()?
        .json()
        .get("content")?
        .as_object()?
        .get(key)?
        .as_str()
        .map(str::to_owned)
}

/// The `m.room_membership` rooms a restricted room's `allow` list names, in order. Empty for a
/// room whose join rule is neither `restricted` nor `knock_restricted`.
fn allowed_room_ids<B: KvBackend>(actor: &RoomActor<B>) -> Vec<OwnedRoomId> {
    if !matches!(
        content_str(actor, "m.room.join_rules", "join_rule").as_deref(),
        Some("restricted" | "knock_restricted")
    ) {
        return Vec::new();
    }
    actor
        .known_allowed_rooms()
        .into_iter()
        .map(|(room, _)| room)
        .collect()
}

/// One room's [`RoomSummary`], from its current state.
///
/// # Errors
/// [`RoomError::State`] if the current state cannot be read.
pub fn summarize<B: KvBackend>(actor: &RoomActor<B>) -> Result<RoomSummary, RoomError> {
    Ok(RoomSummary {
        room_id: actor.room_id().to_owned(),
        name: content_str(actor, "m.room.name", "name"),
        topic: content_str(actor, "m.room.topic", "topic"),
        canonical_alias: content_str(actor, "m.room.canonical_alias", "alias"),
        avatar_url: content_str(actor, "m.room.avatar", "url"),
        num_joined_members: actor.joined_members()?.len() as u64,
        join_rule: content_str(actor, "m.room.join_rules", "join_rule"),
        room_type: actor.creation_type(),
        world_readable: content_str(actor, "m.room.history_visibility", "history_visibility")
            .as_deref()
            == Some("world_readable"),
        guest_can_join: content_str(actor, "m.room.guest_access", "guest_access").as_deref()
            == Some("can_join"),
        allowed_room_ids: allowed_room_ids(actor),
        encryption: content_str(actor, "m.room.encryption", "algorithm"),
        room_version: Some(actor.room_version().as_str().to_owned()),
    })
}

// ------------------------------------------------------------------------------------------
// Visibility
// ------------------------------------------------------------------------------------------

/// Whether a room may be shown to a requester, as far as the room's own state can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// The room's own state settles it: joined or invited, public, knockable, or world-readable.
    Visible,
    /// The room is restricted to members of these rooms, and the requester is not otherwise
    /// entitled: visible only if the requester is in one of them, which the caller checks
    /// against the requester's memberships (or, for a server, its users' memberships).
    IfInAnyOf(Vec<OwnedRoomId>),
    /// Not to be shown.
    Hidden,
}

/// Whether the join rule alone lets anyone see the room: `public`, `knock` and
/// `knock_restricted` rooms are in the spec's list.
fn join_rule_is_open(join_rule: Option<&str>) -> bool {
    matches!(join_rule, Some("public" | "knock" | "knock_restricted"))
}

/// Whether `user` may see the room, per the spec's list for the client endpoint: their
/// membership is `join` or `invite`; the join rule is `public`, `knock` or `knock_restricted`;
/// history visibility is `world_readable`; or the room is restricted and `user` is in one of
/// the rooms it allows ([`Access::IfInAnyOf`]).
#[must_use]
pub fn local_access<B: KvBackend>(actor: &RoomActor<B>, user: &UserId) -> Access {
    let membership = actor
        .state_event("m.room.member", user.as_str())
        .ok()
        .flatten()
        .and_then(|event| {
            event
                .json()
                .get("content")?
                .as_object()?
                .get("membership")?
                .as_str()
                .map(str::to_owned)
        });
    if matches!(membership.as_deref(), Some("join" | "invite")) {
        return Access::Visible;
    }
    state_access(actor)
}

/// Whether `server` may see the room, per the same list applied to a server: it has a joined
/// or invited user in it; the join rule is open; the room is world-readable; or the room is
/// restricted and the server has a user in one of the rooms it allows ([`Access::IfInAnyOf`]).
/// What the federation `/hierarchy` applies before answering about a room.
#[must_use]
pub fn server_access<B: KvBackend>(actor: &RoomActor<B>, server: &str) -> Access {
    if server_has_member(actor, server) {
        return Access::Visible;
    }
    state_access(actor)
}

/// Whether `server` has a user whose membership in the room is `join` or `invite`.
#[must_use]
pub fn server_has_member<B: KvBackend>(actor: &RoomActor<B>, server: &str) -> bool {
    actor.members().is_ok_and(|members| {
        members.iter().any(|member| {
            let of_server = member
                .header()
                .state_key
                .as_deref()
                .and_then(|key| UserId::parse(key).ok())
                .is_some_and(|user| user.server_name().as_str() == server);
            of_server
                && matches!(
                    member
                        .json()
                        .get("content")
                        .and_then(CanonicalJsonValue::as_object)
                        .and_then(|c| c.get("membership"))
                        .and_then(CanonicalJsonValue::as_str),
                    Some("join" | "invite")
                )
        })
    })
}

/// The part of the answer that depends on the room alone.
fn state_access<B: KvBackend>(actor: &RoomActor<B>) -> Access {
    let join_rule = content_str(actor, "m.room.join_rules", "join_rule");
    if join_rule_is_open(join_rule.as_deref()) {
        return Access::Visible;
    }
    if content_str(actor, "m.room.history_visibility", "history_visibility").as_deref()
        == Some("world_readable")
    {
        return Access::Visible;
    }
    let allowed = allowed_room_ids(actor);
    if allowed.is_empty() {
        Access::Hidden
    } else {
        Access::IfInAnyOf(allowed)
    }
}

/// Whether a room another server described may be shown to `user`, from the summary alone: the
/// join rule is open (or missing, which the spec reads as public), the room is world-readable,
/// or it is restricted to a room `user` is in. `None` when the summary cannot settle it and the
/// caller should fall back to what this server knows (an invite it holds, say).
fn remote_access(summary: &Value, joined_rooms: &HashSet<OwnedRoomId>) -> Option<bool> {
    let join_rule = summary.get("join_rule").and_then(Value::as_str);
    if join_rule.is_none()
        || join_rule_is_open(join_rule)
        || summary.get("world_readable").and_then(Value::as_bool) == Some(true)
    {
        return Some(true);
    }
    let allowed = summary
        .get("allowed_room_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|id| RoomId::parse(id).ok());
    for room in allowed {
        if joined_rooms.contains(&room) {
            return Some(true);
        }
    }
    None
}

// ------------------------------------------------------------------------------------------
// Federation seam
// ------------------------------------------------------------------------------------------

/// What another server answers `GET /_matrix/federation/v1/hierarchy/{roomId}` with: the room,
/// its children the answering server would let this one see (summaries, without their own
/// children), and the children it would not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteHierarchyPage {
    /// The room asked about, with its `children_state`.
    pub room: Value,
    /// Summaries of the room's children, each with a `room_id`.
    pub children: Vec<Value>,
    /// Children the answering server says this server cannot see from anywhere; the walk drops
    /// them without asking anyone else.
    pub inaccessible_children: Vec<String>,
}

impl RemoteHierarchyPage {
    /// Reads a page out of the endpoint's JSON body.
    ///
    /// # Errors
    /// [`RoomError::BadRequest`] when `room` is missing or not an object.
    pub fn from_json(body: &Value) -> Result<Self, RoomError> {
        let room = body
            .get("room")
            .filter(|room| room.is_object())
            .cloned()
            .ok_or_else(|| {
                RoomError::BadRequest("a /hierarchy answer without a room object".to_owned())
            })?;
        let children = body
            .get("children")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|child| child.get("room_id").and_then(Value::as_str).is_some())
            .collect();
        let inaccessible_children = body
            .get("inaccessible_children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect();
        Ok(Self {
            room,
            children,
            inaccessible_children,
        })
    }
}

/// The seam through which the walk asks another server about a room this server does not
/// hold. Implemented in `hs-cli` over the federation client, like `crate::backfill::Backfill`;
/// unset (a server with federation off, this crate's tests) means such a room is left out.
#[async_trait]
pub trait RemoteHierarchy: Send + Sync {
    /// `GET /_matrix/federation/v1/hierarchy/{room_id}?suggested_only=` against `destination`.
    ///
    /// # Errors
    /// Whatever the request fails with; the walk logs it and tries the next server.
    async fn fetch(
        &self,
        destination: &str,
        room_id: &RoomId,
        suggested_only: bool,
    ) -> Result<RemoteHierarchyPage, RoomError>;
}

// ------------------------------------------------------------------------------------------
// Pagination
// ------------------------------------------------------------------------------------------

/// One room waiting to be visited.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueEntry {
    room_id: OwnedRoomId,
    via: Vec<String>,
    depth: u32,
    /// The summary the server answering about this room's parent gave for it, usable as-is when
    /// it is not a space (it has no children to walk) or the walk will not go deeper anyway.
    remote_room: Option<Value>,
}

/// What a `next_batch` token stands for: where the walk stopped, and the request it belongs to.
#[derive(Debug, Clone)]
struct Session {
    requester: String,
    root: OwnedRoomId,
    suggested_only: bool,
    max_depth: Option<u32>,
    queue: Vec<QueueEntry>,
    processed: HashSet<OwnedRoomId>,
    created: Instant,
}

/// The pagination sessions this process holds, by token. Tokens are opaque, random and expire
/// after [`TOKEN_VALIDITY`]; a request with an unknown or expired one is answered
/// `M_INVALID_PARAM`, as is one whose `suggested_only` or `max_depth` changed. In cluster mode
/// the sessions live on whichever replica owns the root room's shard (`/rooms/{roomId}/...` is
/// routed there), so a token survives as long as that ownership does; when it moves, the token
/// is unknown and the client starts over.
#[derive(Default)]
pub struct PaginationSessions {
    sessions: Mutex<HashMap<String, Session>>,
}

impl PaginationSessions {
    fn store(&self, session: Session) -> String {
        let token = new_token();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        sessions.retain(|_, s| now.duration_since(s.created) < TOKEN_VALIDITY);
        while sessions.len() >= MAX_SESSIONS {
            let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, s)| s.created)
                .map(|(token, _)| token.clone())
            else {
                break;
            };
            sessions.remove(&oldest);
        }
        sessions.insert(token.clone(), session);
        token
    }

    fn get(&self, token: &str) -> Option<Session> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions
            .get(token)
            .filter(|s| s.created.elapsed() < TOKEN_VALIDITY)
            .cloned()
    }

    /// How many sessions are held, expired ones included until the next store sweeps them.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether no session is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A fresh token: 24 random bytes as lower-case hex, which no client can guess or forge.
fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------------------------------
// The walk
// ------------------------------------------------------------------------------------------

/// One `GET /hierarchy` request, parsed.
#[derive(Debug, Clone)]
pub struct HierarchyRequest {
    /// The space to start at.
    pub root: OwnedRoomId,
    /// Who is asking; every room is filtered for them.
    pub requester: ruma::OwnedUserId,
    /// Only `suggested` children.
    pub suggested_only: bool,
    /// Rooms per page; clamped to [`MAX_LIMIT`].
    pub limit: usize,
    /// How deep below the root to go; `None` for no limit. `Some(0)` is the root alone.
    pub max_depth: Option<u32>,
    /// A `next_batch` from the previous page.
    pub from: Option<String>,
}

/// One page of the walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HierarchyPage {
    /// The rooms of this page, each a `SpaceHierarchyRoomsChunk`, in walk order.
    pub rooms: Vec<Value>,
    /// The token for the next page, when the walk has rooms left.
    pub next_batch: Option<String>,
    /// What this page cost, for the request's log line.
    pub stats: WalkStats,
}

/// What one page of the walk did, for its log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// The deepest room visited, relative to the root.
    pub depth_reached: u32,
    /// Rooms visited and left out because the requester may not see them.
    pub hidden: usize,
    /// Federation `/hierarchy` requests made.
    pub remote_fetches: usize,
    /// Federation requests that failed (a server unreachable, or refusing) and were skipped.
    pub remote_failures: usize,
    /// Rooms not asked about because a server said they were inaccessible, or because no server
    /// could be asked (no hook installed, no `via`).
    pub remote_skipped: usize,
}

/// A room this server holds and has a joined user in, whose state it can therefore vouch for.
/// Synapse's `is_host_joined`: a room held only through an invite or an old membership is asked
/// of its `via` servers instead, with the held copy as the fallback when nobody answers.
async fn resident_handle<B: KvBackend + 'static>(
    registry: &RoomRegistry<B>,
    room_id: &RoomId,
) -> Option<(crate::actor::RoomActorHandle<B>, bool)> {
    let handle = registry.get_or_load(room_id).await.ok()?;
    let joined = handle.query(|actor| actor.local_user_joined()).await;
    Some((handle, joined))
}

/// One held room, summarised for `requester` if they may see it.
async fn summarize_local<B: KvBackend + 'static>(
    registry: &RoomRegistry<B>,
    handle: &crate::actor::RoomActorHandle<B>,
    requester: &UserId,
    suggested_only: bool,
) -> Result<Option<(RoomSummary, Vec<ChildLink>)>, RoomError> {
    let user = requester.to_owned();
    let (access, summary, children) = handle
        .query(move |actor| {
            let access = local_access(actor, &user);
            let summary = summarize(actor)?;
            let children = child_links(actor, suggested_only)?;
            Ok::<_, RoomError>((access, summary, children))
        })
        .await?;
    let visible = match access {
        Access::Visible => true,
        Access::Hidden => false,
        Access::IfInAnyOf(allowed) => {
            let joined = registry.rooms_joined_by_user(requester)?;
            allowed.iter().any(|room| joined.contains(room))
        }
    };
    Ok(visible.then_some((summary, children)))
}

/// A room this server cannot vouch for, asked of the servers in `via`, first answer wins.
async fn fetch_remote(
    hook: &Arc<dyn RemoteHierarchy>,
    entry: &QueueEntry,
    suggested_only: bool,
    own_server: &str,
    stats: &mut WalkStats,
) -> Option<RemoteHierarchyPage> {
    for destination in entry
        .via
        .iter()
        .filter(|server| server.as_str() != own_server)
        .take(MAX_SERVERS_PER_CHILD)
    {
        stats.remote_fetches += 1;
        match hook
            .fetch(destination, &entry.room_id, suggested_only)
            .await
        {
            Ok(page) => {
                tracing::debug!(room_id = %entry.room_id, destination, children = page.children.len(), inaccessible = page.inaccessible_children.len(), "a server described a room of the space");
                return Some(page);
            }
            Err(error) => {
                stats.remote_failures += 1;
                tracing::debug!(room_id = %entry.room_id, destination, %error, "a server could not describe a room of the space; trying the next");
            }
        }
    }
    None
}

/// `children_state` of a remote room's JSON, as links to queue, in the order given (the
/// answering server sorted them). A link without a usable `state_key` or `via` is dropped.
fn remote_child_links(room: &Value) -> Vec<(OwnedRoomId, Vec<String>)> {
    room.get("children_state")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("m.space.child"))
        .filter_map(|event| {
            let room_id = RoomId::parse(event.get("state_key")?.as_str()?)
                .ok()?
                .to_owned();
            let via: Vec<String> = event
                .get("content")?
                .get("via")?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            (!via.is_empty()).then_some((room_id, via))
        })
        .take(MAX_CHILDREN_PER_SPACE)
        .collect()
}

/// A summary JSON with `children_state` present, as the client endpoint requires.
fn with_children_state(mut room: Value) -> Value {
    if let Value::Object(map) = &mut room {
        map.entry("children_state".to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
    }
    room
}

/// The client endpoint: one page of the space tree under `request.root`, filtered for
/// `request.requester`. See the module docs for the order and the cost.
///
/// # Errors
/// - [`RoomError::Forbidden`] when the requester may not see the root (or nobody can describe
///   it): the spec's `403`.
/// - [`RoomError::InvalidParam`] for a `from` token this server does not know, has expired, or
///   that belongs to a request with a different root, requester, `suggested_only` or
///   `max_depth`.
/// - [`RoomError::State`] and [`RoomError::Store`] for a storage failure.
pub async fn walk<B: KvBackend + 'static>(
    registry: &RoomRegistry<B>,
    request: &HierarchyRequest,
) -> Result<HierarchyPage, RoomError> {
    let limit = request.limit.clamp(1, MAX_LIMIT);
    let own_server = registry.server_name().as_str().to_owned();
    let hook = registry.remote_hierarchy_hook().cloned();
    let mut stats = WalkStats::default();

    let (mut queue, mut processed) = match &request.from {
        Some(token) => {
            let session = registry
                .hierarchy_sessions()
                .get(token)
                .filter(|s| {
                    s.requester == request.requester.as_str()
                        && s.root == request.root
                        && s.suggested_only == request.suggested_only
                        && s.max_depth == request.max_depth
                })
                .ok_or_else(|| RoomError::InvalidParam("Unknown pagination token".to_owned()))?;
            (session.queue, session.processed)
        }
        None => (
            vec![QueueEntry {
                room_id: request.root.clone(),
                via: Vec::new(),
                depth: 0,
                remote_room: None,
            }],
            HashSet::new(),
        ),
    };
    let first_page = request.from.is_none();
    let mut rooms: Vec<Value> = Vec::new();

    while rooms.len() < limit {
        let Some(entry) = queue.pop() else { break };
        if !processed.insert(entry.room_id.clone()) {
            continue;
        }
        stats.depth_reached = stats.depth_reached.max(entry.depth);
        let is_root = first_page && entry.depth == 0 && rooms.is_empty();
        let deeper = request.max_depth.is_none_or(|max| entry.depth < max);

        // Where the room's description comes from, and the children it leads to.
        let mut children: Vec<QueueEntry> = Vec::new();
        let mut room_json: Option<Value> = None;
        let held = resident_handle(registry, &entry.room_id).await;
        let mut use_local = matches!(&held, Some((_, true)));

        if !use_local {
            // Not vouched for here: what the parent's server said, if it is enough, else ask.
            let described = match &entry.remote_room {
                Some(remote)
                    if remote.get("room_type").and_then(Value::as_str) != Some("m.space")
                        || !deeper =>
                {
                    Some((remote.clone(), HashMap::new(), HashSet::new()))
                }
                _ => match (&hook, entry.via.is_empty()) {
                    (Some(hook), false) => fetch_remote(
                        hook,
                        &entry,
                        request.suggested_only,
                        &own_server,
                        &mut stats,
                    )
                    .await
                    .map(|page| {
                        let by_id: HashMap<String, Value> = page
                            .children
                            .into_iter()
                            .filter_map(|c| Some((c.get("room_id")?.as_str()?.to_owned(), c)))
                            .collect();
                        let inaccessible: HashSet<String> =
                            page.inaccessible_children.into_iter().collect();
                        (page.room, by_id, inaccessible)
                    }),
                    _ => {
                        if held.is_none() {
                            stats.remote_skipped += 1;
                        }
                        None
                    }
                },
            };
            match described {
                Some((remote, by_id, inaccessible)) => {
                    let joined: HashSet<OwnedRoomId> = registry
                        .rooms_joined_by_user(&request.requester)?
                        .into_iter()
                        .collect();
                    let visible = match remote_access(&remote, &joined) {
                        Some(visible) => visible,
                        // The summary alone cannot say; an invite this server holds can.
                        None => match &held {
                            Some((handle, _)) => {
                                summarize_local(registry, handle, &request.requester, false)
                                    .await?
                                    .is_some()
                            }
                            None => false,
                        },
                    };
                    if visible {
                        for (child, via) in remote_child_links(&remote).into_iter().rev() {
                            if inaccessible.contains(child.as_str()) {
                                stats.remote_skipped += 1;
                                continue;
                            }
                            let remote_room = by_id.get(child.as_str()).cloned();
                            children.push(QueueEntry {
                                room_id: child,
                                via,
                                depth: entry.depth + 1,
                                remote_room,
                            });
                        }
                        room_json = Some(with_children_state(remote));
                    } else {
                        stats.hidden += 1;
                    }
                }
                // Nobody described it; the held copy, if there is one, is better than nothing.
                None => use_local = held.is_some(),
            }
        }

        if use_local {
            let Some((handle, _)) = &held else {
                unreachable!("use_local is only set when the room is held")
            };
            match summarize_local(registry, handle, &request.requester, request.suggested_only)
                .await?
            {
                Some((summary, links)) => {
                    for link in links.iter().take(MAX_CHILDREN_PER_SPACE).rev() {
                        children.push(QueueEntry {
                            room_id: link.room_id.clone(),
                            via: link.via.clone(),
                            depth: entry.depth + 1,
                            remote_room: None,
                        });
                    }
                    room_json = Some(summary.to_json_with_children(&links));
                }
                None => stats.hidden += 1,
            }
        }

        match room_json {
            Some(json) => {
                rooms.push(json);
                if deeper {
                    queue.extend(children);
                }
            }
            None if is_root => {
                return Err(RoomError::Forbidden(format!(
                    "{} is not allowed to view {}",
                    request.requester, request.root
                )));
            }
            None => {}
        }
    }

    let next_batch = (!queue.is_empty()).then(|| {
        registry.hierarchy_sessions().store(Session {
            requester: request.requester.to_string(),
            root: request.root.clone(),
            suggested_only: request.suggested_only,
            max_depth: request.max_depth,
            queue,
            processed,
            created: Instant::now(),
        })
    });
    Ok(HierarchyPage {
        rooms,
        next_batch,
        stats,
    })
}

/// A canonical JSON value as plain JSON.
fn canonical_to_value(value: &CanonicalJsonValue) -> Value {
    serde_json::from_slice(&value.to_canonical_bytes()).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests;
