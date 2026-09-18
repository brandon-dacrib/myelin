//! Mounting `hs-federation`'s transport server on this server's real data.
//!
//! `hs-federation` deliberately defines its own read seams --
//! [`hs_federation::room_source::RoomDataSource`] and
//! [`hs_federation::transport::FederationQuerySource`] -- rather than depending on `hs-room`,
//! `hs-auth` and `hs-e2e` directly (see `crates/hs-federation/src/room_source.rs`'s module doc
//! for why). Until this module existed, the only implementations of those seams were the
//! in-memory fakes that crate's own handler tests use, so every federation read endpoint was
//! written, tested and unmounted. This module is the adapter that closes that gap, and it lives
//! in `hs-cli` for the same reason [`crate::identity`] does: `hs-cli` is the one crate that
//! already depends on every track's crate, so wiring two of them together here adds no new
//! dependency edge anywhere else.
//!
//! # What these adapters will and will not answer
//!
//! Every method applies [`RegistryRoomSource::visible_to`] before returning any room content, as
//! [`RoomDataSource`]'s contract requires. Beyond that, two endpoints answer *less* than the spec
//! allows, deliberately:
//!
//! - **`/state` and `/state_ids` only answer for the room's most recent event.** `hs-room`'s
//!   actor keeps one flat current-state map and has no historical state-at-an-event query, so the
//!   only event whose state it can report correctly is the newest one in the timeline (whose
//!   "state after" *is* the current state). Asked about any older event, this returns
//!   [`RoomSourceError::NotFound`] rather than the current state, because answering a question
//!   about the past with the present is how a remote server ends up with state it cannot detect
//!   is wrong. Lifting this needs the state engine's historical snapshots, tracked in
//!   `docs/status/06-federation.md`.
//! - **`/openid/userinfo` answers nothing**, because no OpenID token is ever issued: the
//!   client-side `POST /user/{userId}/openid/request_token` endpoint does not exist yet, so there
//!   is no token this could resolve and every call is an invalid token.
//!
//! Auth chains are computed by walking `auth_events` transitively from the stored events
//! themselves (bounded by [`MAX_AUTH_CHAIN`]), not from `hs-state`'s chain-cover index: the room
//! actor does not maintain that index yet, and a direct walk over a room's own auth DAG is both
//! correct and cheap at the room sizes this server has been run at.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use hs_federation::room_source::{EventJson, RoomDataSource, RoomSourceError};
use hs_federation::transport::FederationQuerySource;
use hs_kv::KvBackend;
use hs_model::Event;
use hs_room::actor::RoomActor;
use hs_room::registry::RoomRegistry;
use hs_room::routes::render::canonical_to_json;
use hs_room::timeline::{Direction, PaginationToken};
use serde_json::Value;

/// The most events any single auth-chain walk will visit before giving up and returning what it
/// has. A room's auth chain is normally a few dozen events (one create, one power-levels, one
/// join per sender); this exists so a malformed or adversarial `auth_events` graph cannot turn
/// one federation read into an unbounded traversal.
const MAX_AUTH_CHAIN: usize = 2_000;

/// The most timeline events `/timestamp_to_event` will scan backwards through looking for the
/// event closest to a timestamp. Rooms larger than this answer from their most recent
/// [`MAX_TIMESTAMP_SCAN`] events, which is the part of the timeline a `dir=b` caller is looking
/// in anyway.
const MAX_TIMESTAMP_SCAN: usize = 10_000;

// ------------------------------------------------------------------------------------------
// Room data
// ------------------------------------------------------------------------------------------

/// [`RoomDataSource`] over `hs-room`'s [`RoomRegistry`] and `hs-user`'s published-room directory.
pub struct RegistryRoomSource<B: KvBackend> {
    rooms: Arc<RoomRegistry<B>>,
    directory: hs_user::store::DynUserStore,
}

impl<B: KvBackend + 'static> RegistryRoomSource<B> {
    /// Wraps an already-open registry and user store. Both are shared handles: this adapter opens
    /// nothing of its own and sees exactly the data the client-server API sees.
    #[must_use]
    pub fn new(rooms: Arc<RoomRegistry<B>>, directory: hs_user::store::DynUserStore) -> Self {
        Self { rooms, directory }
    }

    /// Loads a room's actor handle. A room ID that does not parse is reported as
    /// [`RoomSourceError::RoomNotFound`] rather than a distinct "malformed" error: to a remote
    /// server asking about a room this server does not have, the two are the same answer, and
    /// keeping them the same avoids handing out a parser oracle.
    async fn handle(
        &self,
        room_id: &str,
    ) -> Result<hs_room::actor::RoomActorHandle<B>, RoomSourceError> {
        let parsed = ruma::RoomId::parse(room_id).map_err(|_| RoomSourceError::RoomNotFound)?;
        self.rooms
            .get_or_load(&parsed)
            .await
            .map_err(|_| RoomSourceError::RoomNotFound)
    }

    /// Runs `f` against a room's actor, having first confirmed the room exists and is visible to
    /// `requesting_server`. Every content-returning method below goes through this, so "checked
    /// visibility before reading" is structural here rather than a rule each method remembers.
    async fn with_visible_room<T, F>(
        &self,
        room_id: &str,
        requesting_server: &str,
        f: F,
    ) -> Result<T, RoomSourceError>
    where
        T: Send + 'static,
        F: FnOnce(&RoomActor<B>) -> Result<T, RoomSourceError> + Send + 'static,
    {
        let handle = self.handle(room_id).await?;
        let server = requesting_server.to_owned();
        handle
            .query(move |actor| {
                if !visible_to(actor, &server) {
                    return Err(RoomSourceError::NotVisible);
                }
                f(actor)
            })
            .await
    }

    /// The pagination token a `/backfill` call should start from: the position of the earliest
    /// event the caller says it already has, plus one, so that event is the first one returned.
    /// An event ID that does not parse, is not in this room, or is a state-only outlier with no
    /// timeline position is ignored; if none is usable the backfill starts at the live end, which
    /// is what the spec says an empty `v` means.
    fn backfill_token(&self, room_id: &str, from_event_ids: &[String]) -> Option<PaginationToken> {
        let mut earliest: Option<i64> = None;
        for id in from_event_ids {
            let Ok(parsed) = ruma::EventId::parse(id) else {
                continue;
            };
            let Ok(Some(row)) = self.rooms.find_event_globally(&parsed) else {
                continue;
            };
            if row.room_id != room_id {
                continue;
            }
            if let Some(pos) = row.room_pos {
                earliest = Some(earliest.map_or(pos, |best: i64| best.min(pos)));
            }
        }
        earliest.map(|pos| PaginationToken::new(pos.saturating_add(1), Direction::Backward))
    }
}

/// Whether `server` may see this room's content at all: it has at least one currently-joined
/// member, or the room's history visibility is `world_readable`.
fn visible_to<B: KvBackend>(actor: &RoomActor<B>, server: &str) -> bool {
    let world_readable = actor
        .state_event("m.room.history_visibility", "")
        .ok()
        .flatten()
        .and_then(|e| content_str(e, "history_visibility").map(str::to_owned))
        .as_deref()
        == Some("world_readable");
    if world_readable {
        return true;
    }
    actor.joined_members().is_ok_and(|members| {
        members.iter().any(|member| {
            member
                .header()
                .state_key
                .as_deref()
                .and_then(server_of)
                .is_some_and(|s| s == server)
        })
    })
}

/// The server-name half of a Matrix user ID (`@user:server` -> `server`).
fn server_of(user_id: &str) -> Option<&str> {
    user_id.split_once(':').map(|(_, server)| server)
}

/// One `content` field of an event, as a string.
fn content_str<'a>(event: &'a Event, key: &str) -> Option<&'a str> {
    event.json().get("content")?.as_object()?.get(key)?.as_str()
}

/// The full PDU JSON a federation response carries: the event exactly as stored and signed,
/// including `hashes`, `signatures`, `auth_events`, `prev_events` and `depth`. This is
/// deliberately *not* `hs_room::routes::render::client_event_json`, which strips all of those and
/// adds `event_id` -- a remote server cannot verify an event whose signature material has been
/// removed, and (for every room version this server creates) must not be sent an `event_id` field
/// at all, since it is not part of the signed object.
fn full_pdu(event: &Event) -> Value {
    canonical_to_json(event.json())
}

/// The event IDs in an event's `auth_events`, handling both the room version 1/2 shape (a
/// `[event_id, hashes]` pair per entry) and the version 3+ shape (a bare event ID string).
fn auth_event_ids(event: &Event) -> Vec<String> {
    let Some(entries) = event.json().get("auth_events").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry.as_str() {
            Some(id) => Some(id.to_owned()),
            None => entry
                .as_array()
                .and_then(|pair| pair.first())
                .and_then(|id| id.as_str())
                .map(str::to_owned),
        })
        .collect()
}

/// The event IDs in an event's `prev_events`, in the same two shapes as [`auth_event_ids`].
fn prev_event_ids(event: &Event) -> Vec<String> {
    let Some(entries) = event.json().get("prev_events").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry.as_str() {
            Some(id) => Some(id.to_owned()),
            None => entry
                .as_array()
                .and_then(|pair| pair.first())
                .and_then(|id| id.as_str())
                .map(str::to_owned),
        })
        .collect()
}

/// Looks one event up in a resident actor by its string ID.
fn event_by_str<'a, B: KvBackend>(actor: &'a RoomActor<B>, event_id: &str) -> Option<&'a Event> {
    let parsed = ruma::EventId::parse(event_id).ok()?;
    actor.event_by_id(&parsed)
}

/// The transitive closure of `roots`' `auth_events`, breadth-first, excluding the roots
/// themselves and bounded by [`MAX_AUTH_CHAIN`]. Events the actor does not hold (an auth event
/// this server never received) are skipped rather than failing the whole call: a partial auth
/// chain is what the requesting server would have to reconstruct anyway, and refusing the whole
/// response because one link is missing helps nobody.
fn auth_chain_from<B: KvBackend>(actor: &RoomActor<B>, roots: &[String]) -> Vec<Value> {
    let mut seen: HashSet<String> = roots.iter().cloned().collect();
    let mut queue: VecDeque<String> = roots.iter().cloned().collect();
    let mut chain = Vec::new();

    while let Some(id) = queue.pop_front() {
        if chain.len() >= MAX_AUTH_CHAIN {
            tracing::warn!(
                room_id = %actor.room_id(),
                "auth chain walk hit its bound; returning a truncated chain"
            );
            break;
        }
        let Some(event) = event_by_str(actor, &id) else {
            continue;
        };
        for next in auth_event_ids(event) {
            if seen.insert(next.clone()) {
                queue.push_back(next);
            }
        }
        // The roots are the events being asked *about*; their auth chain is everything they
        // reach, not including themselves.
        if !roots.contains(&id) {
            chain.push(full_pdu(event));
        }
    }
    chain
}

/// The newest event in the room's timeline, which is the only event whose state this adapter can
/// report (see the module doc).
fn newest_event<B: KvBackend>(actor: &RoomActor<B>) -> Option<String> {
    let (events, _) = actor.paginate(None, Direction::Backward, 1);
    events.first().map(|e| e.event_id().to_string())
}

/// The room's current state, plus the auth chain reachable from it, as `/state` wants them.
fn current_state_with_auth_chain<B: KvBackend>(
    actor: &RoomActor<B>,
) -> Result<(Vec<Value>, Vec<Value>), RoomSourceError> {
    let state = actor
        .full_state()
        .map_err(|_| RoomSourceError::RoomNotFound)?;
    let state_ids: Vec<String> = state.iter().map(|e| e.event_id().to_string()).collect();
    let pdus: Vec<Value> = state.iter().map(|e| full_pdu(e)).collect();
    // `/state`'s `auth_chain` is the auth chain of the state events, which (unlike
    // `/event_auth`'s) conventionally includes the state events themselves: they are what the
    // recipient must authenticate. Walking from the state events' own auth_events and then adding
    // the state back is the same set without double-counting.
    let mut chain = auth_chain_from(actor, &state_ids);
    chain.extend(pdus.iter().cloned());
    Ok((pdus, chain))
}

#[async_trait]
impl<B: KvBackend + 'static> RoomDataSource for RegistryRoomSource<B> {
    async fn is_visible_to(&self, room_id: &str, requesting_server: &str) -> bool {
        let Ok(handle) = self.handle(room_id).await else {
            return false;
        };
        let server = requesting_server.to_owned();
        handle.query(move |actor| visible_to(actor, &server)).await
    }

    async fn get_event(
        &self,
        room_id: &str,
        event_id: &str,
        requesting_server: &str,
    ) -> Result<EventJson, RoomSourceError> {
        let wanted = event_id.to_owned();
        self.with_visible_room(room_id, requesting_server, move |actor| {
            event_by_str(actor, &wanted)
                .map(full_pdu)
                .ok_or(RoomSourceError::NotFound)
        })
        .await
    }

    async fn get_event_by_id(
        &self,
        event_id: &str,
        requesting_server: &str,
    ) -> Result<(String, EventJson), RoomSourceError> {
        // The one lookup that is not room-scoped: `/event/{eventId}`'s path does not name a room,
        // so the room has to be found first -- and then checked, exactly as if the caller had
        // named it.
        let parsed = ruma::EventId::parse(event_id).map_err(|_| RoomSourceError::NotFound)?;
        let row = self
            .rooms
            .find_event_globally(&parsed)
            .map_err(|_| RoomSourceError::NotFound)?
            .ok_or(RoomSourceError::NotFound)?;
        let event = self
            .get_event(&row.room_id, event_id, requesting_server)
            .await?;
        Ok((row.room_id, event))
    }

    async fn state_at(
        &self,
        room_id: &str,
        at_event_id: &str,
        requesting_server: &str,
    ) -> Result<(Vec<EventJson>, Vec<EventJson>), RoomSourceError> {
        let at = at_event_id.to_owned();
        self.with_visible_room(room_id, requesting_server, move |actor| {
            if newest_event(actor).as_deref() != Some(at.as_str()) {
                // Not a lie by omission: see the module doc. The alternative is answering a
                // question about historical state with current state.
                return Err(RoomSourceError::NotFound);
            }
            current_state_with_auth_chain(actor)
        })
        .await
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
        Ok((event_ids_of(&state), event_ids_of(&auth_chain)))
    }

    async fn auth_chain(
        &self,
        room_id: &str,
        event_id: &str,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError> {
        let wanted = event_id.to_owned();
        self.with_visible_room(room_id, requesting_server, move |actor| {
            if event_by_str(actor, &wanted).is_none() {
                return Err(RoomSourceError::NotFound);
            }
            Ok(auth_chain_from(actor, std::slice::from_ref(&wanted)))
        })
        .await
    }

    async fn backfill(
        &self,
        room_id: &str,
        from_event_ids: &[String],
        limit: usize,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError> {
        // `v` names the events the caller already has; backfill walks the timeline backwards from
        // the earliest of them, that event included (the caller holding it does not mean it holds
        // the copy this server would send, and re-sending one event is cheaper than a round trip
        // to discover it was needed). Positions come from the store rather than the actor,
        // because `room_pos` -- what `paginate` is keyed by -- is only carried on the persisted
        // row, not on the in-memory event.
        let token = self.backfill_token(room_id, from_event_ids);
        self.with_visible_room(room_id, requesting_server, move |actor| {
            let (events, _) = actor.paginate(token, Direction::Backward, limit);
            Ok(events.into_iter().map(full_pdu).collect())
        })
        .await
    }

    async fn missing_events(
        &self,
        room_id: &str,
        earliest_events: &[String],
        latest_events: &[String],
        limit: usize,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError> {
        let earliest: HashSet<String> = earliest_events.iter().cloned().collect();
        let latest = latest_events.to_vec();
        self.with_visible_room(room_id, requesting_server, move |actor| {
            // Walk back over `prev_events` from what the caller has, stopping at the events it
            // says it already knows. This is the gap-filling request a remote makes when an
            // event it received cites parents it has never seen.
            let mut seen: HashSet<String> = earliest.clone();
            let mut queue: VecDeque<String> = latest.iter().cloned().collect();
            let mut found = Vec::new();
            while let Some(id) = queue.pop_front() {
                if found.len() >= limit {
                    break;
                }
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Some(event) = event_by_str(actor, &id) else {
                    continue;
                };
                if !latest.contains(&id) {
                    found.push(full_pdu(event));
                }
                for prev in prev_event_ids(event) {
                    if !seen.contains(&prev) {
                        queue.push_back(prev);
                    }
                }
            }
            Ok(found)
        })
        .await
    }

    async fn event_near_timestamp(
        &self,
        room_id: &str,
        ts: u64,
        dir_forward: bool,
        requesting_server: &str,
    ) -> Result<(String, u64), RoomSourceError> {
        self.with_visible_room(room_id, requesting_server, move |actor| {
            let (events, _) = actor.paginate(None, Direction::Backward, MAX_TIMESTAMP_SCAN);
            let target = i64::try_from(ts).unwrap_or(i64::MAX);
            let best = events
                .into_iter()
                .filter(|event| {
                    let at = event.header().origin_server_ts;
                    if dir_forward {
                        at >= target
                    } else {
                        at <= target
                    }
                })
                .min_by_key(|event| (event.header().origin_server_ts - target).abs());
            best.map(|event| {
                (
                    event.event_id().to_string(),
                    u64::try_from(event.header().origin_server_ts).unwrap_or(0),
                )
            })
            .ok_or(RoomSourceError::NotFound)
        })
        .await
    }

    async fn hierarchy(
        &self,
        room_id: &str,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError> {
        self.with_visible_room(room_id, requesting_server, move |actor| {
            let state = actor
                .full_state()
                .map_err(|_| RoomSourceError::RoomNotFound)?;
            Ok(state
                .into_iter()
                .filter(|event| event.header().event_type == "m.space.child")
                .map(full_pdu)
                .collect())
        })
        .await
    }

    async fn public_room_summary(&self, room_id: &str) -> Option<(Option<String>, bool)> {
        let entries = self.directory.list_public_rooms().await.ok()?;
        entries
            .into_iter()
            .find(|entry| entry.room_id.as_str() == room_id)
            .map(|entry| (entry.canonical_alias, true))
    }

    async fn list_public_rooms(&self, limit: usize, since: Option<&str>) -> Vec<EventJson> {
        let Ok(mut entries) = self.directory.list_public_rooms().await else {
            return Vec::new();
        };
        // Same ordering and `since`-as-an-offset convention the client-server `/publicRooms`
        // handler uses (`hs_user::routes::rooms`), so a room's position in the directory does not
        // depend on which API asked.
        entries.sort_by(|a, b| a.room_id.cmp(&b.room_id));
        let offset: usize = since.and_then(|s| s.parse().ok()).unwrap_or(0);
        entries
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|entry| {
                serde_json::json!({
                    "room_id": entry.room_id,
                    "name": entry.name,
                    "topic": entry.topic,
                    "canonical_alias": entry.canonical_alias,
                    "avatar_url": entry.avatar_url,
                    "num_joined_members": entry.num_joined_members,
                    "world_readable": entry.world_readable,
                    "guest_can_join": entry.guest_can_join,
                    "join_rule": "public",
                })
            })
            .collect()
    }
}

/// The `event_id` of every event in a rendered PDU list. Federation PDUs for room version 3+ do
/// not carry `event_id`, so this recomputes nothing: it is only used on lists this adapter built
/// from events it holds, where the ID is already known.
fn event_ids_of(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| {
            event
                .get("event_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

// ------------------------------------------------------------------------------------------
// Non-room queries
// ------------------------------------------------------------------------------------------

/// [`FederationQuerySource`] over this server's auth store (users and devices), `hs-e2e`'s device
/// key store and `hs-room`'s alias keyspace.
pub struct ServerQuerySource<B: KvBackend> {
    auth: Arc<dyn hs_auth::store::AuthStore>,
    e2e: Arc<dyn hs_e2e::store::E2eStore>,
    rooms: Arc<RoomRegistry<B>>,
    own_server_name: String,
}

impl<B: KvBackend + 'static> ServerQuerySource<B> {
    /// Wraps already-open stores.
    #[must_use]
    pub fn new(
        auth: Arc<dyn hs_auth::store::AuthStore>,
        e2e: Arc<dyn hs_e2e::store::E2eStore>,
        rooms: Arc<RoomRegistry<B>>,
        own_server_name: impl Into<String>,
    ) -> Self {
        Self {
            auth,
            e2e,
            rooms,
            own_server_name: own_server_name.into(),
        }
    }
}

#[async_trait]
impl<B: KvBackend + 'static> FederationQuerySource for ServerQuerySource<B> {
    async fn profile(&self, user_id: &str, field: Option<&str>) -> Option<Value> {
        let parsed = ruma::UserId::parse(user_id).ok()?;
        // An existing local user with nothing set gets an empty profile, not a 404: "this user
        // exists and has no display name" and "no such user" are different answers, and this
        // server has no profile storage at all yet (no `/profile` route exists on the
        // client-server side either), so every local user is the first case.
        self.auth.get_user(&parsed).await.ok().flatten()?;
        match field {
            Some("displayname" | "avatar_url") | None => Some(serde_json::json!({})),
            Some(_) => None,
        }
    }

    async fn resolve_alias(&self, alias: &str) -> Option<(String, Vec<String>)> {
        let parsed = ruma::RoomAliasId::parse(alias).ok()?;
        let room_id = self.rooms.resolve_alias(&parsed).ok().flatten()?;
        // The only server known to be in the room from here is this one; a fuller answer would
        // list every server with a joined member, which `/query/directory`'s callers treat as a
        // hint rather than a guarantee.
        Some((room_id.to_string(), vec![self.own_server_name.clone()]))
    }

    async fn devices(&self, user_id: &str) -> Option<Value> {
        let parsed = ruma::UserId::parse(user_id).ok()?;
        self.auth.get_user(&parsed).await.ok().flatten()?;
        let keys = self.e2e.list_device_keys(&parsed).await.ok()?;
        let stream_id = self.e2e.current_stream_pos().await.ok()?;
        let devices: Vec<Value> = keys
            .into_iter()
            .map(|(device_id, row)| {
                serde_json::json!({
                    "device_id": device_id,
                    "keys": row.keys,
                })
            })
            .collect();
        Some(serde_json::json!({
            "user_id": user_id,
            "stream_id": stream_id,
            "devices": devices,
        }))
    }

    async fn openid_userinfo(&self, _access_token: &str) -> Option<String> {
        // No OpenID token is ever issued (see the module doc), so every token presented here is
        // one this server did not mint.
        None
    }
}

// ------------------------------------------------------------------------------------------
// Fetching remote servers' keys
// ------------------------------------------------------------------------------------------

/// A [`hs_federation::keys::KeyServerFetcher`] that fetches `/_matrix/key/v2/server` through the
/// real outbound [`hs_federation::client::FederationClient`], so inbound `X-Matrix` verification
/// can obtain the keys of a server it has never spoken to. Without this, every inbound federation
/// request fails verification (no key, no check) -- which is why mounting the transport server
/// requires an outbound client even for a deployment that only ever receives.
pub struct ClientKeyFetcher {
    client: Arc<hs_federation::client::FederationClient>,
}

impl ClientKeyFetcher {
    /// Wraps an outbound client.
    #[must_use]
    pub fn new(client: Arc<hs_federation::client::FederationClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl hs_federation::keys::KeyServerFetcher for ClientKeyFetcher {
    async fn fetch_server_key(&self, server_name: &str) -> Option<Value> {
        match self
            .client
            .send(server_name, "GET", "/_matrix/key/v2/server", None)
            .await
        {
            Ok(response) if response.status == 200 => Some(response.body),
            Ok(response) => {
                tracing::debug!(
                    server = server_name,
                    status = response.status,
                    "key server returned a non-200 response"
                );
                None
            }
            Err(error) => {
                tracing::debug!(server = server_name, %error, "could not fetch a server's keys");
                None
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// Assembling the mount
// ------------------------------------------------------------------------------------------

/// Everything `hs serve` needs to mount federation: the transport server's state, the context its
/// `X-Matrix` verification layer runs against, and this server's own signing keys (which the
/// unauthenticated `/_matrix/key/v2/server` endpoint publishes).
pub struct FederationMount {
    /// The state every transport handler runs against.
    pub state: hs_federation::transport::FederationState,
    /// The verification context the `X-Matrix` layer uses: this server's name, plus the cache of
    /// remote servers' keys that inbound signatures are checked against.
    pub x_matrix: Arc<hs_federation::xmatrix::XMatrixContext>,
    /// This server's own signing keys, in the form the key server publishes them.
    pub own_keys: Arc<hs_federation::keys::OwnSigningKeys>,
    /// This server's name, as it appears in the key response it signs.
    pub server_name: String,
    /// The outbound client, kept alive because the inbound key fetcher borrows it (and because a
    /// caller that later adds a sender needs exactly this one, sharing its backoff state).
    pub client: Arc<hs_federation::client::FederationClient>,
}

/// How long a `/_matrix/key/v2/server` response this server signs stays valid. The spec caps what
/// a *requesting* server may trust at seven days; a shorter window than that costs one cheap
/// refetch per day and shortens the period a compromised-then-rotated key stays accepted.
const KEY_VALIDITY_SECS: u64 = 24 * 60 * 60;

/// Builds the federation mount over this server's already-open stores.
///
/// The signing keys published here are the *same* keys `hs-room` stamps onto events
/// ([`crate::identity`] loads them once, and this takes a copy), not a second set loaded
/// independently: a server that signed its events with one key and advertised another would have
/// every event it ever sent rejected, and the two loaders drifting apart is exactly how that
/// happens.
///
/// # Errors
/// Returns the backend's error if the destination-backoff keyspace cannot be opened.
pub fn build_mount<B: KvBackend + 'static>(
    config: &hs_config::Config,
    identity: &hs_room::identity::HomeserverIdentity,
    backend: B,
    rooms: Arc<RoomRegistry<B>>,
    directory: hs_user::store::DynUserStore,
    auth: Arc<dyn hs_auth::store::AuthStore>,
    e2e: Arc<dyn hs_e2e::store::E2eStore>,
) -> Result<FederationMount, hs_kv::KvError> {
    let server_name = identity.server_name.to_string();
    let own_keys = Arc::new(hs_federation::keys::OwnSigningKeys::from_keys(vec![
        (*identity.signing_key).clone(),
    ]));

    let destinations = Arc::new(hs_federation::destination_store::KvDestinationStore::open(
        backend,
    )?);
    let well_known = Arc::new(hs_federation::discovery::CachingWellKnownFetcher::new(
        hs_federation::discovery::HttpWellKnownFetcher::new(),
    ));
    let (srv, addr) = resolvers();

    let client = Arc::new(hs_federation::client::FederationClient::new(
        server_name.clone(),
        (*identity.signing_key).clone(),
        client_config(&config.federation),
        destinations,
        well_known,
        srv,
        addr,
    ));

    let key_cache: Arc<hs_federation::keys::DynRemoteKeyCache> = Arc::new(
        hs_federation::keys::RemoteKeyCache::new(Box::new(ClientKeyFetcher::new(client.clone()))
            as Box<dyn hs_federation::keys::KeyServerFetcher>),
    );

    let state = hs_federation::transport::FederationState {
        own_server_name: Arc::from(server_name.as_str()),
        rooms: Arc::new(RegistryRoomSource::new(rooms.clone(), directory)),
        queries: Arc::new(ServerQuerySource::new(
            auth,
            e2e,
            rooms,
            server_name.clone(),
        )),
        allow_public_rooms_over_federation: config.federation.allow_public_rooms_over_federation,
        allow_device_name_lookup_over_federation: config
            .federation
            .allow_device_name_lookup_over_federation,
    };

    let x_matrix = Arc::new(hs_federation::xmatrix::XMatrixContext {
        own_server_name: server_name.clone(),
        key_cache,
    });

    Ok(FederationMount {
        state,
        x_matrix,
        own_keys,
        server_name,
        client,
    })
}

/// `hs-config`'s federation settings, in the shape the outbound client takes them.
fn client_config(config: &hs_config::FederationConfig) -> hs_federation::client::ClientConfig {
    hs_federation::client::ClientConfig {
        enabled: config.enabled,
        domain_policy: hs_federation::client::DomainPolicy::new(config.domain_allowlist.clone()),
        ip_policy: hs_federation::client::IpPolicy::from_cidrs(
            &config.ip_range_blocklist,
            &config.ip_range_allowlist,
        ),
        verify_certificates: config.verify_certificates,
        request_timeout: config.client_timeout.into(),
        max_retry_backoff: config.max_retry_backoff.into(),
        ..hs_federation::client::ClientConfig::default()
    }
}

/// The system DNS resolver, or -- if the system has no usable resolver configuration -- a pair
/// that resolves nothing. Federation is then inbound-only in practice: nothing outbound can be
/// addressed, including the key fetches that inbound verification needs, so inbound requests from
/// servers whose keys are not already cached will fail to verify. That is a loud warning and a
/// degraded server, not a failed boot: a homeserver with no DNS should still serve its local
/// users.
fn resolvers() -> (
    Arc<dyn hs_federation::discovery::SrvResolver>,
    Arc<dyn hs_federation::discovery::AddrResolver>,
) {
    match hs_federation::discovery::HickoryResolver::from_system_conf() {
        Ok(resolver) => {
            let resolver = Arc::new(resolver);
            (resolver.clone(), resolver)
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "no usable system DNS configuration; federation cannot resolve any remote server"
            );
            let none = Arc::new(NoResolver);
            (none.clone(), none)
        }
    }
}

/// The resolver used when the system has none: every lookup returns nothing.
struct NoResolver;

#[async_trait]
impl hs_federation::discovery::SrvResolver for NoResolver {
    async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
        Vec::new()
    }
}

#[async_trait]
impl hs_federation::discovery::AddrResolver for NoResolver {
    async fn resolve_addr(&self, _hostname: &str) -> Vec<std::net::IpAddr> {
        Vec::new()
    }
}

/// The `GET /_matrix/key/v2/server` response: this server's name, its current verify keys, and a
/// `valid_until_ts` [`KEY_VALIDITY_SECS`] out, signed with every one of those keys. Rebuilt per
/// request rather than cached, so `valid_until_ts` is always fresh -- signing one small object is
/// far cheaper than the TLS handshake that carried the request.
///
/// # Errors
/// Returns [`hs_federation::error::FederationError`] if signing fails, which for an object this
/// function builds itself should not happen.
pub fn server_key_response(
    server_name: &str,
    own_keys: &hs_federation::keys::OwnSigningKeys,
) -> Result<Value, hs_federation::error::FederationError> {
    // No key has ever been rotated out by this server (nothing rotates keys yet), so
    // `old_verify_keys` is empty rather than unimplemented.
    hs_federation::keys::build_server_key_response(server_name, own_keys, &[], KEY_VALIDITY_SECS)
}

/// A mount whose data sources hold nothing and whose key cache can fetch nothing, for
/// [`crate::serve::route_manifest`]: the federation router's *routes* are static, so reading them
/// off does not need real stores, a real DNS resolver or a real signing key -- and building it
/// this way keeps `hs routes-manifest` free of the file and network I/O a real mount performs.
#[must_use]
pub fn manifest_only_mount() -> (
    hs_federation::transport::FederationState,
    Arc<hs_federation::xmatrix::XMatrixContext>,
) {
    let state = hs_federation::transport::FederationState {
        own_server_name: Arc::from("routes-manifest.invalid"),
        rooms: Arc::new(hs_federation::room_source::InMemoryRoomSource::new()),
        queries: Arc::new(hs_federation::transport::InMemoryQuerySource::default()),
        allow_public_rooms_over_federation: false,
        allow_device_name_lookup_over_federation: false,
    };
    let key_cache: Arc<hs_federation::keys::DynRemoteKeyCache> =
        Arc::new(hs_federation::keys::RemoteKeyCache::new(
            Box::new(NoKeyFetcher) as Box<dyn hs_federation::keys::KeyServerFetcher>,
        ));
    let x_matrix = Arc::new(hs_federation::xmatrix::XMatrixContext {
        own_server_name: "routes-manifest.invalid".to_string(),
        key_cache,
    });
    (state, x_matrix)
}

/// The key fetcher [`manifest_only_mount`] uses: it never fetches anything, because nothing ever
/// calls a handler built from that mount.
struct NoKeyFetcher;

#[async_trait]
impl hs_federation::keys::KeyServerFetcher for NoKeyFetcher {
    async fn fetch_server_key(&self, _server_name: &str) -> Option<Value> {
        None
    }
}
