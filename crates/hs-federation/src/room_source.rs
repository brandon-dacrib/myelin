//! The `RoomDataSource` seam: everything the federation transport server's read/query endpoints
//! need to know about a room, defined by this crate rather than borrowed from `hs-room`'s actor
//! internals.
//!
//! See `docs/design/06-federation-threat-model.md` section 5 for why this trait exists here
//! instead of a direct dependency on `hs-room`: inbound federation reads (like inbound federation
//! writes) must not be built against the room actor's current flat state map, which is only
//! correct for the single-writer, no-fork case — a federation read endpoint that silently
//! degrades to "whatever the flat map currently holds" is not a defensible foundation for the
//! per-room defences in threat model section 2.4. A real adapter onto `hs-room`'s query surface
//! (`RoomActorHandle::query`) is later work, tracked in this crate's status file under
//! "Interfaces needed"; [`InMemoryRoomSource`] is a fake used only by this crate's own handler
//! tests until then.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;

/// Why a `RoomDataSource` lookup failed, distinguishing "nothing to see" from "something exists
/// but you may not see it" only where [`RoomDataSource`]'s own doc says the spec requires the
/// distinction (see threat model 2.4's "existence/enumeration oracles" — most federation read
/// endpoints deliberately do *not* distinguish these to a non-member, and callers in
/// `crate::transport` must consult the per-endpoint list, not multiplex on this enum for every
/// case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomSourceError {
    /// No such room.
    RoomNotFound,
    /// The room exists, but the requesting server is not entitled to see it (not a joined member,
    /// and the room is not world-readable).
    NotVisible,
    /// The room exists and is visible, but the specific item asked for (an event ID, a state key)
    /// does not exist.
    NotFound,
}

/// One row of a `/state` or `/state_ids`-shaped response: a full event, or just its ID (the
/// caller picks which via the two different trait methods rather than this type branching, to
/// keep the size-shape of each response method's return value obvious at the call site).
pub type EventJson = Value;

/// The read-only surface a federation transport handler needs from "the room store", scoped
/// exactly to what `docs/design/06-federation-threat-model.md` section 2.4 enumerates. Every
/// method takes `requesting_server` explicitly so an implementation can (and must) apply the
/// membership/visibility check described in that section before returning anything — there is no
/// method that returns room content without also being told who is asking.
#[async_trait]
pub trait RoomDataSource: Send + Sync {
    /// Whether `requesting_server` may see this room's content at all: it has at least one
    /// currently-joined member belonging to that server, or the room's join rule makes it
    /// world-readable to federation. This is the gate every other method below must apply before
    /// returning content (threat model 2.4's "room membership check bypass").
    async fn is_visible_to(&self, room_id: &str, requesting_server: &str) -> bool;

    /// Fetches one event by ID, if it exists, the room exists, and it is visible to
    /// `requesting_server`.
    async fn get_event(
        &self,
        room_id: &str,
        event_id: &str,
        requesting_server: &str,
    ) -> Result<EventJson, RoomSourceError>;

    /// Fetches one event by ID alone (for `/event/{eventId}`, which the spec does not scope by
    /// room in its path — the server has to know which room an event belongs to). Returns
    /// `(room_id, event)`.
    async fn get_event_by_id(
        &self,
        event_id: &str,
        requesting_server: &str,
    ) -> Result<(String, EventJson), RoomSourceError>;

    /// The full current state of the room (for `/state`), plus the auth chain of the event named
    /// by `at_event_id`. Returns `(state_events, auth_chain_events)`.
    async fn state_at(
        &self,
        room_id: &str,
        at_event_id: &str,
        requesting_server: &str,
    ) -> Result<(Vec<EventJson>, Vec<EventJson>), RoomSourceError>;

    /// Just the event IDs for `/state_ids`: `(state_event_ids, auth_chain_event_ids)`.
    async fn state_ids_at(
        &self,
        room_id: &str,
        at_event_id: &str,
        requesting_server: &str,
    ) -> Result<(Vec<String>, Vec<String>), RoomSourceError>;

    /// The auth chain of one event, for `/event_auth`.
    async fn auth_chain(
        &self,
        room_id: &str,
        event_id: &str,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError>;

    /// Backfill: up to `limit` events (already clamped by the caller to the resource-limits
    /// table) walking backwards from `from_event_ids`.
    async fn backfill(
        &self,
        room_id: &str,
        from_event_ids: &[String],
        limit: usize,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError>;

    /// `/get_missing_events`: events reachable from `latest_events` but not in
    /// `earliest_events`, depth-limited and count-limited (both already clamped by the caller).
    async fn missing_events(
        &self,
        room_id: &str,
        earliest_events: &[String],
        latest_events: &[String],
        limit: usize,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError>;

    /// The single event whose `origin_server_ts` is closest to `ts`, honouring `dir` (`"b"` for
    /// the closest event at or before `ts`, `"f"` for at or after), for `/timestamp_to_event`.
    async fn event_near_timestamp(
        &self,
        room_id: &str,
        ts: u64,
        dir_forward: bool,
        requesting_server: &str,
    ) -> Result<(String, u64), RoomSourceError>;

    /// The room's `m.space.child`-derived hierarchy summary rooted at `room_id`, for `/hierarchy`.
    /// Each entry is a `(room_id, state_json)` pair the caller serializes into the spec's
    /// `m.space.child`/room-summary shape; the *filtering* to "rooms this requester may see" is
    /// this method's job, not the caller's.
    async fn hierarchy(
        &self,
        room_id: &str,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError>;

    /// The room's join rule / world-readability-relevant profile info for `/query/directory` and
    /// `/publicRooms`-shaped calls: `(canonical_alias, is_public)`.
    async fn public_room_summary(&self, room_id: &str) -> Option<(Option<String>, bool)>;

    /// Every room this server advertises publicly, for `/publicRooms`, already paginated by the
    /// caller (this returns one page's worth — the caller clamps page size per the resource
    /// limits table).
    async fn list_public_rooms(&self, limit: usize, since: Option<&str>) -> Vec<EventJson>;

    /// The room's version, if this server hosts it at all. Not gated by
    /// [`RoomDataSource::is_visible_to`]: a room's version is needed *before* a join handshake can
    /// even begin (to pick a compatible `make_join` template), for a caller who by definition is
    /// not yet a member.
    async fn room_version(&self, room_id: &str) -> Option<String>;

    /// The room's current forward extremities as `(event_id, depth)` pairs, in the order a new
    /// event's `prev_events` should cite them. Used by `make_join` to build a join template's
    /// `prev_events`/`depth`. Ordinarily exactly one entry: nothing that can currently reach this
    /// server's rooms introduces a second one (see `crate::join`'s module doc for the same
    /// assumption made explicit).
    async fn forward_extremities(
        &self,
        room_id: &str,
    ) -> Result<Vec<(String, i64)>, RoomSourceError>;

    /// The room's current state (with each event's ID alongside it, since a stored PDU for room
    /// version 3+ does not itself carry `event_id`) plus that state's auth chain, for the join
    /// handshake. Deliberately **not** gated by [`RoomDataSource::is_visible_to`]: handing a
    /// prospective joiner's server the state and auth chain it needs to construct and authorize a
    /// join event is the entire purpose of `make_join`/`send_join`, not a bypass of the membership
    /// check that gates ordinary reads -- the join itself is authorized separately, by
    /// `hs_state::auth::check_event_auth`, before any of this is ever returned to a caller over the
    /// wire.
    async fn state_for_join(&self, room_id: &str) -> Result<StateForJoin, RoomSourceError>;
}

/// The current state plus its auth chain, as `send_join`/`make_join` need it. See
/// [`RoomDataSource::state_for_join`].
#[derive(Debug, Clone, Default)]
pub struct StateForJoin {
    /// `(event_id, event_json)` for every event in the room's current state.
    pub state: Vec<(String, EventJson)>,
    /// The auth chain of that state (every event's `auth_events`, transitively, excluding the
    /// state events themselves unless they are also reached that way).
    pub auth_chain: Vec<EventJson>,
}

/// An in-memory [`RoomDataSource`] for this crate's own handler tests. Not a production
/// implementation: rooms and their members/events/state are inserted directly by test setup code,
/// with no persistence, no state resolution, and no auth-chain computation beyond what a test
/// wires up by hand.
#[derive(Default)]
pub struct InMemoryRoomSource {
    rooms: HashMap<String, FakeRoom>,
}

#[derive(Default, Clone)]
pub struct FakeRoom {
    pub world_readable: bool,
    pub joined_servers: Vec<String>,
    pub events: HashMap<String, EventJson>,
    pub state: Vec<EventJson>,
    pub auth_chain: HashMap<String, Vec<EventJson>>,
    pub public: bool,
    pub canonical_alias: Option<String>,
    /// `room_version`, for [`RoomDataSource::room_version`] and the join handshake tests.
    pub room_version: Option<String>,
    /// [`RoomDataSource::forward_extremities`]'s answer.
    pub extremities: Vec<(String, i64)>,
    /// The auth chain [`RoomDataSource::state_for_join`] returns alongside `state`. Kept separate
    /// from `auth_chain` (which is keyed per-event, for `/event_auth`) since a join handshake asks
    /// for the chain of the whole current state at once, not of one named event.
    pub join_auth_chain: Vec<EventJson>,
}

impl InMemoryRoomSource {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_room(&mut self, room_id: impl Into<String>, room: FakeRoom) {
        self.rooms.insert(room_id.into(), room);
    }
}

#[async_trait]
impl RoomDataSource for InMemoryRoomSource {
    async fn is_visible_to(&self, room_id: &str, requesting_server: &str) -> bool {
        self.rooms.get(room_id).is_some_and(|r| {
            r.world_readable || r.joined_servers.iter().any(|s| s == requesting_server)
        })
    }

    async fn get_event(
        &self,
        room_id: &str,
        event_id: &str,
        requesting_server: &str,
    ) -> Result<EventJson, RoomSourceError> {
        let room = self
            .rooms
            .get(room_id)
            .ok_or(RoomSourceError::RoomNotFound)?;
        if !self.is_visible_to(room_id, requesting_server).await {
            return Err(RoomSourceError::NotVisible);
        }
        room.events
            .get(event_id)
            .cloned()
            .ok_or(RoomSourceError::NotFound)
    }

    async fn get_event_by_id(
        &self,
        event_id: &str,
        requesting_server: &str,
    ) -> Result<(String, EventJson), RoomSourceError> {
        for (room_id, room) in &self.rooms {
            if let Some(event) = room.events.get(event_id) {
                if !self.is_visible_to(room_id, requesting_server).await {
                    return Err(RoomSourceError::NotVisible);
                }
                return Ok((room_id.clone(), event.clone()));
            }
        }
        Err(RoomSourceError::NotFound)
    }

    async fn state_at(
        &self,
        room_id: &str,
        at_event_id: &str,
        requesting_server: &str,
    ) -> Result<(Vec<EventJson>, Vec<EventJson>), RoomSourceError> {
        let room = self
            .rooms
            .get(room_id)
            .ok_or(RoomSourceError::RoomNotFound)?;
        if !self.is_visible_to(room_id, requesting_server).await {
            return Err(RoomSourceError::NotVisible);
        }
        let auth_chain = room
            .auth_chain
            .get(at_event_id)
            .cloned()
            .unwrap_or_default();
        Ok((room.state.clone(), auth_chain))
    }

    async fn state_ids_at(
        &self,
        room_id: &str,
        at_event_id: &str,
        requesting_server: &str,
    ) -> Result<(Vec<String>, Vec<String>), RoomSourceError> {
        let (state, auth_chain) = self
            .state_at(room_id, at_event_id, requesting_server)
            .await?;
        Ok((extract_ids(&state), extract_ids(&auth_chain)))
    }

    async fn auth_chain(
        &self,
        room_id: &str,
        event_id: &str,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError> {
        let room = self
            .rooms
            .get(room_id)
            .ok_or(RoomSourceError::RoomNotFound)?;
        if !self.is_visible_to(room_id, requesting_server).await {
            return Err(RoomSourceError::NotVisible);
        }
        Ok(room.auth_chain.get(event_id).cloned().unwrap_or_default())
    }

    async fn backfill(
        &self,
        room_id: &str,
        from_event_ids: &[String],
        limit: usize,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError> {
        let room = self
            .rooms
            .get(room_id)
            .ok_or(RoomSourceError::RoomNotFound)?;
        if !self.is_visible_to(room_id, requesting_server).await {
            return Err(RoomSourceError::NotVisible);
        }
        let _ = from_event_ids;
        Ok(room.events.values().take(limit).cloned().collect())
    }

    async fn missing_events(
        &self,
        room_id: &str,
        earliest_events: &[String],
        latest_events: &[String],
        limit: usize,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError> {
        let room = self
            .rooms
            .get(room_id)
            .ok_or(RoomSourceError::RoomNotFound)?;
        if !self.is_visible_to(room_id, requesting_server).await {
            return Err(RoomSourceError::NotVisible);
        }
        let _ = (earliest_events, latest_events);
        Ok(room.events.values().take(limit).cloned().collect())
    }

    async fn event_near_timestamp(
        &self,
        room_id: &str,
        ts: u64,
        _dir_forward: bool,
        requesting_server: &str,
    ) -> Result<(String, u64), RoomSourceError> {
        let room = self
            .rooms
            .get(room_id)
            .ok_or(RoomSourceError::RoomNotFound)?;
        if !self.is_visible_to(room_id, requesting_server).await {
            return Err(RoomSourceError::NotVisible);
        }
        room.events
            .iter()
            .next()
            .map(|(id, _)| (id.clone(), ts))
            .ok_or(RoomSourceError::NotFound)
    }

    async fn hierarchy(
        &self,
        room_id: &str,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError> {
        let room = self
            .rooms
            .get(room_id)
            .ok_or(RoomSourceError::RoomNotFound)?;
        if !self.is_visible_to(room_id, requesting_server).await {
            return Err(RoomSourceError::NotVisible);
        }
        Ok(room.state.clone())
    }

    async fn public_room_summary(&self, room_id: &str) -> Option<(Option<String>, bool)> {
        self.rooms
            .get(room_id)
            .map(|r| (r.canonical_alias.clone(), r.public))
    }

    async fn list_public_rooms(&self, limit: usize, _since: Option<&str>) -> Vec<EventJson> {
        self.rooms
            .values()
            .filter(|r| r.public)
            .take(limit)
            .map(|r| serde_json::json!({ "canonical_alias": r.canonical_alias }))
            .collect()
    }

    async fn room_version(&self, room_id: &str) -> Option<String> {
        self.rooms.get(room_id)?.room_version.clone()
    }

    async fn forward_extremities(
        &self,
        room_id: &str,
    ) -> Result<Vec<(String, i64)>, RoomSourceError> {
        let room = self
            .rooms
            .get(room_id)
            .ok_or(RoomSourceError::RoomNotFound)?;
        Ok(room.extremities.clone())
    }

    async fn state_for_join(&self, room_id: &str) -> Result<StateForJoin, RoomSourceError> {
        let room = self
            .rooms
            .get(room_id)
            .ok_or(RoomSourceError::RoomNotFound)?;
        let state = room
            .state
            .iter()
            .filter_map(|event| {
                event
                    .get("event_id")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_owned(), event.clone()))
            })
            .collect();
        Ok(StateForJoin {
            state,
            auth_chain: room.join_auth_chain.clone(),
        })
    }
}

fn extract_ids(events: &[EventJson]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| {
            e.get("event_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room_with_member(server: &str) -> FakeRoom {
        FakeRoom {
            world_readable: false,
            joined_servers: vec![server.to_string()],
            events: HashMap::new(),
            state: Vec::new(),
            auth_chain: HashMap::new(),
            public: false,
            canonical_alias: None,
            ..FakeRoom::default()
        }
    }

    #[tokio::test]
    async fn visibility_requires_membership_when_not_world_readable() {
        let mut src = InMemoryRoomSource::new();
        src.insert_room("!r:example.org", room_with_member("member.example.org"));

        assert!(
            src.is_visible_to("!r:example.org", "member.example.org")
                .await
        );
        assert!(
            !src.is_visible_to("!r:example.org", "outsider.example.org")
                .await
        );
    }

    #[tokio::test]
    async fn world_readable_room_is_visible_to_anyone() {
        let mut src = InMemoryRoomSource::new();
        let room = FakeRoom {
            world_readable: true,
            ..FakeRoom::default()
        };
        src.insert_room("!r:example.org", room);
        assert!(
            src.is_visible_to("!r:example.org", "anyone.example.org")
                .await
        );
    }

    #[tokio::test]
    async fn get_event_denies_non_members_before_returning_any_content() {
        let mut src = InMemoryRoomSource::new();
        let mut room = room_with_member("member.example.org");
        room.events.insert(
            "$e1".to_string(),
            serde_json::json!({"event_id": "$e1", "type": "m.room.message"}),
        );
        src.insert_room("!r:example.org", room);

        let err = src
            .get_event("!r:example.org", "$e1", "outsider.example.org")
            .await
            .unwrap_err();
        assert_eq!(err, RoomSourceError::NotVisible);

        let ok = src
            .get_event("!r:example.org", "$e1", "member.example.org")
            .await
            .unwrap();
        assert_eq!(ok["event_id"], "$e1");
    }

    #[tokio::test]
    async fn missing_room_is_room_not_found_not_not_visible() {
        let src = InMemoryRoomSource::new();
        let err = src
            .get_event("!nope:example.org", "$e1", "anyone.example.org")
            .await
            .unwrap_err();
        assert_eq!(err, RoomSourceError::RoomNotFound);
    }
}
