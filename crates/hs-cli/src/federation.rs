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
//! [`RoomDataSource`]'s contract requires. Two endpoints are worth a note:
//!
//! - `/state` and `/state_ids` used to refuse every event but the newest, because `hs-room` exposed no
//!   historical state query and answering about the past with the present would have handed a
//!   remote state it could not detect was wrong. `hs_room::actor::RoomActor::state_before_event`
//!   lifted that: both endpoints now answer for any event this server holds in its timeline,
//!   with the state *before* the event, as the spec and Synapse (`get_state_ids_for_pdu`) do --
//!   what a server backfilling from this one derives the state at a batch from
//!   (`crate::backfill`). They answered with the state *after* it until 2026-09-30, which
//!   differs whenever the event is itself a state event. An outlier this server has not placed
//!   in its timeline has no known state and is `404`, as in Synapse.
//! - **`/openid/userinfo`** resolves the tokens `POST /user/{userId}/openid/request_token`
//!   issues (`hs_auth::openid::userinfo`); any other token is `401`.
//!
//! Auth chains are computed by walking `auth_events` transitively from the stored events
//! themselves (bounded by [`MAX_AUTH_CHAIN`]), not from `hs-state`'s chain-cover index: the room
//! actor does not maintain that index yet, and a direct walk over a room's own auth DAG is both
//! correct and cheap at the room sizes this server has been run at.

use std::collections::{HashMap, HashSet, VecDeque};
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

/// [`RoomDataSource`] over `hs-room`'s [`RoomRegistry`], its published-room directory included.
pub struct RegistryRoomSource<B: KvBackend> {
    rooms: Arc<RoomRegistry<B>>,
    /// The account store, to tell an erased account's events
    /// ([`RegistryRoomSource::with_erasure`]). `None` serves every event as held.
    accounts: Option<Arc<dyn hs_auth::store::AuthStore>>,
}

impl<B: KvBackend + 'static> RegistryRoomSource<B> {
    /// Wraps an already-open registry and user store. Both are shared handles: this adapter opens
    /// nothing of its own and sees exactly the data the client-server API sees.
    #[must_use]
    pub fn new(rooms: Arc<RoomRegistry<B>>) -> Self {
        Self {
            rooms,
            accounts: None,
        }
    }

    /// Serves the events of this server's erased accounts (`hs_auth::erasure`) to other servers
    /// in their redacted form, through `/event`, `/backfill` and `/get_missing_events`: an
    /// erased account's messages are not handed out again, as Synapse's
    /// `filter_events_for_server` prunes an erased sender's events (Sytest's "Inbound federation
    /// redacts events from erased users"). The events stay whole in the room, and to its
    /// members here; what clients see of them is the room layer's.
    #[must_use]
    pub fn with_erasure(mut self, accounts: Arc<dyn hs_auth::store::AuthStore>) -> Self {
        self.accounts = Some(accounts);
        self
    }

    /// `pdus` of `room_id`, each of an erased account of this server's replaced by its redacted
    /// form. See [`RegistryRoomSource::with_erasure`].
    async fn without_erased_content(&self, room_id: &str, pdus: Vec<Value>) -> Vec<Value> {
        let Some(accounts) = &self.accounts else {
            return pdus;
        };
        let Ok(handle) = self.handle(room_id).await else {
            return pdus;
        };
        let version = handle.query(|actor| actor.room_version().clone()).await;
        let own = self.rooms.server_name().to_owned();
        let Some(rules) = hs_model::room_version::rules_for(&version) else {
            return pdus;
        };
        let mut erased: HashMap<String, bool> = HashMap::new();
        let mut out = Vec::with_capacity(pdus.len());
        for pdu in pdus {
            let Some(sender) = pdu
                .get("sender")
                .and_then(Value::as_str)
                .and_then(|s| ruma::UserId::parse(s).ok())
            else {
                out.push(pdu);
                continue;
            };
            if sender.server_name() != own {
                out.push(pdu);
                continue;
            }
            let is_erased = match erased.get(sender.as_str()) {
                Some(known) => *known,
                None => {
                    let known = matches!(
                        accounts.get_user(&sender).await,
                        Ok(Some(record)) if record.erased
                    );
                    erased.insert(sender.to_string(), known);
                    known
                }
            };
            if !is_erased {
                out.push(pdu);
                continue;
            }
            let redacted = hs_model::canonical::to_canonical_object(&pdu, false)
                .ok()
                .and_then(|object| hs_model::redaction::redact(&object, &rules.redaction).ok());
            match redacted {
                Some(redacted) => {
                    tracing::debug!(room_id, %sender, "serving an erased account's event redacted");
                    out.push(canonical_to_json(&redacted));
                }
                None => {
                    tracing::warn!(room_id, %sender, "could not redact an erased account's event; leaving it out");
                }
            }
        }
        out
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

    /// One room of a space as `/hierarchy` describes it to `server`: its summary and its
    /// `m.space.child` links, or `None` when `server` may not see it
    /// (`hs_room::hierarchy::server_access`). A restricted room's answer needs the allowed
    /// rooms' members, so those rooms are loaded and asked whether `server` has a user in them.
    async fn space_room_for(
        &self,
        handle: &hs_room::actor::RoomActorHandle<B>,
        server: &str,
        suggested_only: bool,
    ) -> Result<
        Option<(
            hs_room::hierarchy::RoomSummary,
            Vec<hs_room::hierarchy::ChildLink>,
        )>,
        RoomSourceError,
    > {
        use hs_room::hierarchy::{Access, child_links, server_access, summarize};
        let server_owned = server.to_owned();
        let (access, summary, links) = handle
            .query(move |actor| {
                let access = server_access(actor, &server_owned);
                let summary = summarize(actor).map_err(|_| RoomSourceError::RoomNotFound)?;
                let links = child_links(actor, suggested_only)
                    .map_err(|_| RoomSourceError::RoomNotFound)?;
                Ok::<_, RoomSourceError>((access, summary, links))
            })
            .await?;
        let visible = match access {
            Access::Visible => true,
            Access::Hidden => false,
            Access::IfInAnyOf(allowed) => {
                let mut found = false;
                for room in allowed {
                    let Ok(allowed_handle) = self.handle(room.as_str()).await else {
                        continue;
                    };
                    let server_owned = server.to_owned();
                    if allowed_handle
                        .query(move |actor| {
                            hs_room::hierarchy::server_has_member(actor, &server_owned)
                        })
                        .await
                    {
                        found = true;
                        break;
                    }
                }
                found
            }
        };
        Ok(visible.then_some((summary, links)))
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
///
/// A redacted event is served in its redacted form -- still hashed and signed, still a PDU the
/// requester can verify (its content hash then fails, and the spec has the receiver redact it,
/// which it already is) -- as Synapse serves it: another server is not handed what was taken
/// back. The redacted form keeps no `unsigned`, so the redaction this server names in it for its
/// own clients (`unsigned.redacted_because`, `hs_room::actor::redactions`) never leaves either;
/// Synapse strips both from a PDU too. Until 2026-10-01 a redacted event went out whole.
fn full_pdu(event: &Event) -> Value {
    if event.header().flags.is_redacted()
        && let Ok(redacted) = event.redacted_json()
    {
        return canonical_to_json(&redacted);
    }
    canonical_to_json(event.json())
}

/// `event` as `requesting_server` may have it: whole if the room's history visibility as of the
/// event lets that server see it (`RoomActor::server_may_see`), its redacted form otherwise --
/// still signed, still hashed, still a PDU the requester can verify and place, just without
/// the content it was not there for. `None` only if the event cannot be redacted at all (no
/// `type`, a non-object `content`), which a stored event never is; leaving such an event out is
/// the safe side of a visibility gate.
fn pdu_for_server<B: KvBackend>(
    actor: &RoomActor<B>,
    event: &Event,
    requesting_server: &str,
) -> Option<Value> {
    match actor.server_may_see(event, requesting_server) {
        Ok(true) => Some(full_pdu(event)),
        Ok(false) => {
            let rules = hs_model::room_version::rules_for(actor.room_version())?;
            match hs_model::redaction::redact(event.json(), &rules.redaction) {
                Ok(redacted) => Some(canonical_to_json(&redacted)),
                Err(error) => {
                    tracing::warn!(event_id = %event.event_id(), %error, "could not redact an event for a server that may not see it whole; leaving it out");
                    None
                }
            }
        }
        Err(error) => {
            tracing::warn!(event_id = %event.event_id(), %error, "could not decide whether a server may see an event; leaving it out");
            None
        }
    }
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
    actor.held_event(&parsed)
}

/// The transitive closure of `roots`' `auth_events`, breadth-first and bounded by
/// [`MAX_AUTH_CHAIN`]: every event reachable through an `auth_events` edge, a root only when
/// another root reaches it. Events the actor does not hold (an auth event
/// this server never received) are skipped rather than failing the whole call: a partial auth
/// chain is what the requesting server would have to reconstruct anyway, and refusing the whole
/// response because one link is missing helps nobody.
fn auth_chain_from<B: KvBackend>(actor: &RoomActor<B>, roots: &[String]) -> Vec<Value> {
    // Every event reached through an `auth_events` edge from a root is in the chain -- a root
    // included, when another root cites it. The roots are the events being asked *about*, so
    // one that no root cites is not its own ancestor and stays out; but a room's state is
    // mostly its own auth events (the create event, the power levels, the creator's join), and
    // leaving every root out answered `send_join` with an empty auth chain for a room whose auth
    // events were all current state (Sytest's "Inbound federation can receive v1/v2
    // /send_join", until 2026-10-01). Synapse's `get_auth_chain_ids` answers the same set.
    let mut reached: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = roots
        .iter()
        .filter_map(|id| event_by_str(actor, id))
        .flat_map(auth_event_ids)
        .collect();
    let mut chain = Vec::new();

    while let Some(id) = queue.pop_front() {
        if !reached.insert(id.clone()) {
            continue;
        }
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
        queue.extend(
            auth_event_ids(event)
                .into_iter()
                .filter(|next| !reached.contains(next)),
        );
        chain.push(full_pdu(event));
    }
    chain
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
        let pdu = self
            .with_visible_room(room_id, requesting_server, move |actor| {
                event_by_str(actor, &wanted)
                    .map(full_pdu)
                    .ok_or(RoomSourceError::NotFound)
            })
            .await?;
        self.without_erased_content(room_id, vec![pdu])
            .await
            .pop()
            .ok_or(RoomSourceError::NotFound)
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
            let (state, chain) = state_before_with_chain(actor, &at)?;
            Ok((
                state.iter().map(full_pdu).collect(),
                chain.iter().map(full_pdu).collect(),
            ))
        })
        .await
    }

    async fn state_ids_at(
        &self,
        room_id: &str,
        at_event_id: &str,
        requesting_server: &str,
    ) -> Result<(Vec<String>, Vec<String>), RoomSourceError> {
        // From the events, not their rendered PDUs: a PDU of room version 3 or later carries
        // no `event_id` (it is derived from the event's hash), so reading it back out of the
        // JSON answered every such room with two empty lists until 2026-09-30.
        let at = at_event_id.to_owned();
        self.with_visible_room(room_id, requesting_server, move |actor| {
            let (state, chain) = state_before_with_chain(actor, &at)?;
            let ids = |events: &[hs_model::Event]| -> Vec<String> {
                events.iter().map(|e| e.event_id().to_string()).collect()
            };
            Ok((ids(&state), ids(&chain)))
        })
        .await
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
        if token.is_none() && !from_event_ids.is_empty() {
            // None of `v` is an event of this room (one of another room, or one this server
            // has never seen): there is nothing to walk back from, and the answer is empty,
            // not the room's newest history (Sytest's "Backfill checks the events requested
            // belong to the room").
            self.with_visible_room(room_id, requesting_server, |_| Ok(()))
                .await?;
            tracing::info!(
                room_id,
                requesting_server,
                v = ?from_event_ids,
                "backfill asked from events that are not this room's; answering none"
            );
            return Ok(Vec::new());
        }
        let server = requesting_server.to_owned();
        let pdus = self
            .with_visible_room(room_id, requesting_server, move |actor| {
                let (events, _) = actor.paginate(token, Direction::Backward, limit);
                Ok(events
                    .into_iter()
                    .filter_map(|event| pdu_for_server(actor, event, &server))
                    .collect())
            })
            .await?;
        Ok(self.without_erased_content(room_id, pdus).await)
    }

    async fn missing_events(
        &self,
        room_id: &str,
        earliest_events: &[String],
        latest_events: &[String],
        limit: usize,
        min_depth: i64,
        requesting_server: &str,
    ) -> Result<Vec<EventJson>, RoomSourceError> {
        let earliest: HashSet<String> = earliest_events.iter().cloned().collect();
        let latest = latest_events.to_vec();
        let server = requesting_server.to_owned();
        let pdus = self
            .with_visible_room(room_id, requesting_server, move |actor| {
                // Walk back over `prev_events` from what the caller has, stopping at the events it
                // says it already knows. This is the gap-filling request a remote makes when an
                // event it received cites parents it has never seen.
                let mut seen: HashSet<String> = earliest.clone();
                let mut queue: VecDeque<String> = latest.iter().cloned().collect();
                let mut found: Vec<&hs_model::Event> = Vec::new();
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
                    // Below the caller's floor: not returned, and not walked past either, since
                    // everything behind it is shallower still.
                    if event.header().depth < min_depth {
                        continue;
                    }
                    if !latest.contains(&id) {
                        found.push(event);
                    }
                    for prev in prev_event_ids(event) {
                        if !seen.contains(&prev) {
                            queue.push_back(prev);
                        }
                    }
                }
                // The walk above runs backwards, so `found` is newest-first -- but the response has
                // to be oldest-first, the order the events happened in. A requesting server replays
                // them into its own DAG, and one that reads the first entry as the earliest of the
                // batch gets the wrong event: Complement reads `*ev.StateKey()` off it and
                // dereferences a nil pointer when it is a message rather than a state event, which
                // kills the whole Go test binary and silently discards every test after it.
                //
                // Depth, then event ID, because depth alone is not a total order: two events on
                // forked branches share one, and the response must still be stable for a caller
                // comparing two servers' answers.
                found.sort_by(|a, b| {
                    a.header()
                        .depth
                        .cmp(&b.header().depth)
                        .then_with(|| a.event_id().as_str().cmp(b.event_id().as_str()))
                });
                Ok(found
                    .into_iter()
                    .filter_map(|event| pdu_for_server(actor, event, &server))
                    .collect())
            })
            .await?;
        Ok(self.without_erased_content(room_id, pdus).await)
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
        suggested_only: bool,
        requesting_server: &str,
    ) -> Result<EventJson, RoomSourceError> {
        // Not `with_visible_room`: the spec's list for who may see a room in a space is wider
        // than "a member or world-readable" (a public or knockable room, a restricted room the
        // server has a user in), and is `hs_room::hierarchy::server_access`'s.
        let root = self.handle(room_id).await?;
        let Some((summary, links)) = self
            .space_room_for(&root, requesting_server, suggested_only)
            .await?
        else {
            return Err(RoomSourceError::NotVisible);
        };
        let mut children = Vec::new();
        let mut inaccessible_children = Vec::new();
        for link in links
            .iter()
            .take(hs_room::hierarchy::MAX_CHILDREN_PER_SPACE)
        {
            // A child this server does not hold is left out, not listed as inaccessible: the
            // asking server may know a server that does.
            let Ok(child) = self.handle(link.room_id.as_str()).await else {
                continue;
            };
            match self
                .space_room_for(&child, requesting_server, suggested_only)
                .await?
            {
                Some((summary, _)) => children
                    .push(serde_json::to_value(summary).map_err(|_| RoomSourceError::NotFound)?),
                None => inaccessible_children.push(link.room_id.to_string()),
            }
        }
        tracing::debug!(
            room_id,
            requesting_server,
            suggested_only,
            children = children.len(),
            inaccessible = inaccessible_children.len(),
            "answered a federation hierarchy request"
        );
        Ok(serde_json::json!({
            "room": summary.to_json_with_children(&links),
            "children": children,
            "inaccessible_children": inaccessible_children,
        }))
    }

    async fn public_room_summary(&self, room_id: &str) -> Option<(Option<String>, bool)> {
        let parsed = ruma::RoomId::parse(room_id).ok()?;
        if !self.rooms.is_directory_public(&parsed).ok()? {
            return None;
        }
        let handle = self.handle(room_id).await.ok()?;
        let entry = handle
            .query(hs_room::routes::directory::public_rooms_chunk_entry)
            .await;
        let alias = entry
            .get("canonical_alias")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Some((alias, true))
    }

    async fn list_public_rooms(&self, limit: usize, since: Option<&str>) -> Vec<EventJson> {
        // The rooms published to the directory (`PUT /directory/list/room/{roomId}`,
        // `createRoom`'s `visibility: "public"`): the same flag and the same entry shape the
        // client-server `/publicRooms` answers (`hs_room::routes::directory`), so another
        // server sees what a local client sees. Until 2026-10-02 this read `hs-user`'s
        // join-rule proxy, which listed public-join rooms whether published or not and never a
        // published invite-only one (Sytest's "Inbound federation can get public room list").
        let Ok(mut room_ids) = self.rooms.list_published_room_ids() else {
            return Vec::new();
        };
        room_ids.sort();
        let offset: usize = since.and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut chunk = Vec::new();
        for room_id in room_ids.into_iter().skip(offset).take(limit) {
            // Unpublished and evicted between the scan and this load: skip it, not the listing.
            let Ok(handle) = self.handle(room_id.as_str()).await else {
                continue;
            };
            chunk.push(
                handle
                    .query(hs_room::routes::directory::public_rooms_chunk_entry)
                    .await,
            );
        }
        chunk
    }

    async fn room_version(&self, room_id: &str) -> Option<String> {
        let handle = self.handle(room_id).await.ok()?;
        Some(
            handle
                .query(|actor| actor.room_version().as_str().to_owned())
                .await,
        )
    }

    async fn forward_extremities(
        &self,
        room_id: &str,
    ) -> Result<Vec<(String, i64)>, RoomSourceError> {
        // The actor's own set, not the newest timeline event: a room this server's user joined
        // elsewhere, an event accepted over `/send` that forked, and a batch of fetched history
        // all put the two apart, and `/get_missing_events` is asked with these as the oldest
        // end of a gap, so they have to be what a new event here would actually cite.
        let handle = self.handle(room_id).await?;
        handle
            .query(|actor| {
                Ok(actor
                    .forward_extremity_ids()
                    .into_iter()
                    .map(|(id, depth)| (id.to_string(), depth))
                    .collect())
            })
            .await
    }

    async fn state_for_join(
        &self,
        room_id: &str,
    ) -> Result<hs_federation::room_source::StateForJoin, RoomSourceError> {
        let handle = self.handle(room_id).await?;
        handle
            .query(|actor| {
                let state = actor
                    .full_state()
                    .map_err(|_| RoomSourceError::RoomNotFound)?;
                let ids: Vec<String> = state
                    .iter()
                    .map(|event| event.event_id().to_string())
                    .collect();
                let state_pairs: Vec<(String, Value)> = state
                    .iter()
                    .map(|event| (event.event_id().to_string(), full_pdu(event)))
                    .collect();
                let auth_chain = auth_chain_from(actor, &ids);
                Ok(hs_federation::room_source::StateForJoin {
                    state: state_pairs,
                    auth_chain,
                })
            })
            .await
    }

    async fn member_servers(&self, room_id: &str) -> Vec<String> {
        let Ok(handle) = self.handle(room_id).await else {
            return Vec::new();
        };
        handle.query(|actor| joined_servers(actor)).await
    }

    async fn server_acl(&self, room_id: &str) -> Option<Value> {
        // Every room-scoped federation request asks this, so one state lookup rather than the
        // whole state `state_for_join` renders.
        let handle = self.handle(room_id).await.ok()?;
        handle
            .query(|actor| {
                actor
                    .state_event("m.room.server_acl", "")
                    .ok()
                    .flatten()
                    .and_then(|event| full_pdu(event).get("content").cloned())
            })
            .await
    }

    async fn event_for_reference(&self, room_id: &str, event_id: &str) -> Option<Value> {
        let handle = self.handle(room_id).await.ok()?;
        let wanted = event_id.to_owned();
        handle
            .query(move |actor| event_by_str(actor, &wanted).map(full_pdu))
            .await
    }

    async fn membership_of(&self, room_id: &str, user_id: &str) -> Option<String> {
        let handle = self.handle(room_id).await.ok()?;
        let user_id = user_id.to_owned();
        handle
            .query(move |actor| {
                actor.full_state().ok()?.iter().find_map(|event| {
                    let header = event.header();
                    (header.event_type == "m.room.member"
                        && header.state_key.as_deref() == Some(user_id.as_str()))
                    .then(|| {
                        full_pdu(event)
                            .get("content")
                            .and_then(|content| content.get("membership"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .flatten()
                })
            })
            .await
    }
}

/// The server of every currently joined member, deduplicated and sorted. Empty if the state
/// store cannot be read: a `send_join` that cannot find out who else is in the room forwards to
/// nobody rather than failing the join it has already stored.
fn joined_servers<B: KvBackend>(actor: &RoomActor<B>) -> Vec<String> {
    let Ok(members) = actor.joined_members() else {
        return Vec::new();
    };
    let servers: std::collections::BTreeSet<String> = members
        .iter()
        .filter_map(|member| member.header().state_key.as_deref().and_then(server_of))
        .map(str::to_owned)
        .collect();
    servers.into_iter().collect()
}

/// The `event_id` of every event in a rendered PDU list. Federation PDUs for room version 3+ do
/// not carry `event_id`, so this recomputes nothing: it is only used on lists this adapter built
/// from events it holds, where the ID is already known.
/// The room's state immediately before `at` (`RoomActor::state_before_event`) and that state's
/// auth chain *with the state events themselves in it*: `/state`'s `auth_chain` conventionally
/// includes them -- they are what the recipient has to authenticate -- while
/// `RoomActor::state_before_event`'s is strictly their ancestors.
fn state_before_with_chain<B: KvBackend>(
    actor: &RoomActor<B>,
    at: &str,
) -> Result<(Vec<hs_model::Event>, Vec<hs_model::Event>), RoomSourceError> {
    let event_id = ruma::EventId::parse(at).map_err(|_| RoomSourceError::NotFound)?;
    // A rejected event is held (so that it is known again) but never accepted: there is no
    // state "at" it to serve, as `/event` does not serve it (Sytest's "/state[_ids] returns
    // M_NOT_FOUND for a rejected message/state event"). Nor at an outlier, an event with no
    // timeline position (a join's state, a missing prev event held with a fetched state):
    // Synapse answers `404` for one ("/state[_ids] returns M_NOT_FOUND for an outlier"), though
    // this server holds a state for some (`hs_room::actor::fetched_state`).
    if actor.is_rejected_event(&event_id) || actor.timeline_position(&event_id).is_none() {
        return Err(RoomSourceError::NotFound);
    }
    let at_state = actor
        .state_before_event(&event_id)
        .map_err(|_| RoomSourceError::NotFound)?
        .ok_or(RoomSourceError::NotFound)?;
    let mut chain = at_state.auth_chain;
    let already: HashSet<ruma::OwnedEventId> = chain
        .iter()
        .map(|event| event.event_id().to_owned())
        .collect();
    for event in &at_state.state {
        if !already.contains(event.event_id()) {
            chain.push(event.clone());
        }
    }
    Ok((at_state.state, chain))
}

/// [`hs_federation::inbound::RoomWriteSink`] over `hs-room`'s [`RoomRegistry`].
///
/// This is the honest edge named throughout `hs_federation::inbound` and `hs_federation::join`'s
/// module docs: it can only ever report success for an event this server already holds (checked
/// by event ID against the resident room actor). `hs-room`'s `RoomActor` has no API to accept an
/// already-built, foreign-signed [`hs_model::Event`] -- `send_event`/`send_event_citing` only
/// build and sign *new*, locally-originated events (see `crates/hs-room/src/actor.rs`, and
/// `crates/hs-room/src/pipeline.rs`'s own doc comment: "The general three-snapshot check the spec
/// requires for an *inbound* federation event ... is still track 06's job"). Closing this needs a
/// new `hs-room` entry point -- see `docs/status/06-federation.md` for the RFC this session wrote
/// describing exactly what it would need to do.
pub struct RegistryWriteSink<B: KvBackend> {
    rooms: Arc<RoomRegistry<B>>,
}

impl<B: KvBackend + 'static> RegistryWriteSink<B> {
    /// Wraps an already-open registry.
    #[must_use]
    pub fn new(rooms: Arc<RoomRegistry<B>>) -> Self {
        Self { rooms }
    }

    async fn handle(&self, room_id: &str) -> Option<hs_room::actor::RoomActorHandle<B>> {
        let parsed = ruma::RoomId::parse(room_id).ok()?;
        self.rooms.get_or_load(&parsed).await.ok()
    }
}

#[async_trait]
impl<B: KvBackend + 'static> hs_federation::inbound::RoomWriteSink for RegistryWriteSink<B> {
    async fn accept_verified_event(
        &self,
        room_id: &str,
        event_id: &str,
        event_json: &Value,
    ) -> Result<hs_federation::inbound::WriteOutcome, hs_federation::inbound::WriteRejected> {
        use hs_federation::inbound::{WriteOutcome, WriteRejected};

        let Some(handle) = self.handle(room_id).await else {
            return Err(WriteRejected::other("unknown room"));
        };
        let Ok(parsed_event_id) = ruma::EventId::parse(event_id) else {
            return Err(WriteRejected::other("malformed event id"));
        };
        let known = handle
            .query(move |actor| actor.event_by_id(&parsed_event_id).is_some())
            .await;
        if known {
            return Ok(WriteOutcome::AlreadyKnown);
        }

        // The event has already had its content hash and signature checked by
        // `hs_federation::inbound::verify_pdu` before reaching this sink -- that is the contract
        // `hs_room::actor::RoomActor::accept_remote_event` documents for its caller, and it
        // deliberately does not redo that work. What it does do is authorize the event against
        // the state its own `auth_events` imply and the state resolved from its `prev_events`,
        // then store it byte-identically, signatures and all.
        let room_version = handle.query(|actor| actor.room_version().clone()).await;
        let event = hs_model::Event::parse(event_json, room_version).map_err(|e| {
            WriteRejected::other(format!(
                "event is not parseable at this room's version: {e}"
            ))
        })?;

        let out_of_room = event.clone();
        match handle.accept_remote_event(event).await {
            // A soft-failed event (`hs_room::actor::soft_fail`) was received and processed:
            // held, in the graph, out of every client read. `/send` answers `{}` for it.
            Ok(
                hs_room::actor::RemoteEventOutcome::Stored(_)
                | hs_room::actor::RemoteEventOutcome::SoftFailed(_),
            ) => Ok(WriteOutcome::Stored),
            Ok(hs_room::actor::RemoteEventOutcome::AlreadyKnown) => Ok(WriteOutcome::AlreadyKnown),
            // The end of an invite or a knock, sent here by a server in a room this server is
            // not in: nothing it cites is held, and nothing ever will be. See
            // `out_of_room_ending`.
            Err(hs_room::RoomError::MissingAncestors(_))
                if out_of_room_ending(&handle, &out_of_room).await =>
            {
                match handle.accept_out_of_room_membership(out_of_room).await {
                    Ok(
                        hs_room::actor::RemoteEventOutcome::Stored(_)
                        | hs_room::actor::RemoteEventOutcome::SoftFailed(_),
                    ) => Ok(WriteOutcome::Stored),
                    Ok(hs_room::actor::RemoteEventOutcome::AlreadyKnown) => {
                        Ok(WriteOutcome::AlreadyKnown)
                    }
                    Err(e) => Err(WriteRejected::other(e.to_string())),
                }
            }
            // A missing ancestor is not a rejection of this event: it means this server has a hole
            // in the DAG and must backfill before the event can be authorized at all. The IDs are
            // carried structurally (not just interpolated into the message) so
            // `hs_federation::backfill::resolve_missing_ancestors` can act on them directly rather
            // than parsing this string back apart.
            Err(hs_room::RoomError::MissingAncestors(ids)) => {
                let id_strings: Vec<String> = ids.iter().map(ToString::to_string).collect();
                Err(WriteRejected::missing_ancestors(
                    id_strings.clone(),
                    format!(
                        "missing {} ancestor event(s) this server has not backfilled yet: {}",
                        id_strings.len(),
                        id_strings.join(", ")
                    ),
                ))
            }
            // Event authorization refused it: processed and rejected, which `/send` answers
            // `{}` for (`WriteRejected::auth_rejected`).
            Err(e @ hs_room::RoomError::Forbidden(_)) => Err(WriteRejected::auth(e.to_string())),
            Err(e) => Err(WriteRejected::other(e.to_string())),
        }
    }

    async fn unknown_events(&self, room_id: &str, event_ids: &[String]) -> Vec<String> {
        let Some(handle) = self.handle(room_id).await else {
            return event_ids.to_vec();
        };
        let ids: Vec<ruma::OwnedEventId> = event_ids
            .iter()
            .filter_map(|id| ruma::OwnedEventId::try_from(id.as_str()).ok())
            .collect();
        handle
            .query(move |actor| actor.events_not_held(&ids))
            .await
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }

    async fn accept_auth_outliers(
        &self,
        room_id: &str,
        events: &[Value],
    ) -> Result<usize, hs_federation::inbound::WriteRejected> {
        use hs_federation::inbound::WriteRejected;

        let Some(handle) = self.handle(room_id).await else {
            return Err(WriteRejected::other("unknown room"));
        };
        let room_version = handle.query(|actor| actor.room_version().clone()).await;
        let parsed: Vec<hs_model::Event> = events
            .iter()
            .filter_map(|raw| hs_model::Event::parse(raw, room_version.clone()).ok())
            .collect();
        handle
            .accept_auth_outliers(parsed)
            .await
            .map_err(|e| WriteRejected::other(e.to_string()))
    }

    async fn accept_prev_event_with_state(
        &self,
        room_id: &str,
        prev_event_id: &str,
        prev_event: &Value,
        state_before: &[String],
        fetched: &[Value],
    ) -> Result<hs_federation::inbound::WriteOutcome, hs_federation::inbound::WriteRejected> {
        use hs_federation::inbound::{WriteOutcome, WriteRejected};

        let Some(handle) = self.handle(room_id).await else {
            return Err(WriteRejected::other("unknown room"));
        };
        let room_version = handle.query(|actor| actor.room_version().clone()).await;
        let prev = hs_model::Event::parse(prev_event, room_version.clone()).map_err(|e| {
            WriteRejected::other(format!(
                "the missing prev event is not parseable at this room's version: {e}"
            ))
        })?;
        if prev.event_id() != prev_event_id {
            return Err(WriteRejected::other(format!(
                "the event fetched for {prev_event_id} is {}",
                prev.event_id()
            )));
        }
        let state_before: Vec<ruma::OwnedEventId> = state_before
            .iter()
            .filter_map(|id| ruma::OwnedEventId::try_from(id.as_str()).ok())
            .collect();
        let mut events = Vec::with_capacity(fetched.len());
        for raw in fetched {
            match hs_model::Event::parse(raw, room_version.clone()) {
                Ok(event) => events.push(event),
                Err(error) => {
                    tracing::debug!(room_id, %error, "an event fetched for a state is not parseable at this room's version; left out");
                }
            }
        }
        match handle
            .accept_prev_event_with_state(prev, state_before, events)
            .await
        {
            Ok(hs_room::actor::RemoteEventOutcome::Stored(_)) => Ok(WriteOutcome::Stored),
            Ok(hs_room::actor::RemoteEventOutcome::SoftFailed(_)) => Ok(WriteOutcome::Stored),
            Ok(hs_room::actor::RemoteEventOutcome::AlreadyKnown) => Ok(WriteOutcome::AlreadyKnown),
            Err(e @ hs_room::RoomError::Forbidden(_)) => Err(WriteRejected::auth(e.to_string())),
            Err(e) => Err(WriteRejected::other(e.to_string())),
        }
    }
}

/// Whether `event`, received over `/send` for a room nobody of this server is joined to, ends a
/// local user's invite or knock: a `leave` or `ban` about a user of this server, whose previous
/// membership here is `invite` or `knock`, sent by a user of a server this server knows to be in
/// the room (`RoomActor::servers_to_join_through`: the inviter's, or the room's own). The
/// inviter rescinding an invite and a resident refusing a knock both arrive this way, since the
/// target's server is sent membership changes about its own users; neither can be placed in a
/// room this server does not hold, so they are recorded out of band
/// (`RoomActor::accept_out_of_room_membership`). An invite never comes this way -- that is
/// `PUT /invite`'s job -- and a server nobody here has heard of cannot end one.
async fn out_of_room_ending<B: KvBackend + 'static>(
    handle: &hs_room::actor::RoomActorHandle<B>,
    event: &hs_model::Event,
) -> bool {
    let header = event.header();
    if header.event_type != "m.room.member" {
        return false;
    }
    let membership = event
        .json()
        .get("content")
        .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
        .and_then(|content| content.get("membership"))
        .and_then(hs_model::canonical::CanonicalJsonValue::as_str);
    if !matches!(membership, Some("leave" | "ban")) {
        return false;
    }
    let Some(target) = header.state_key.clone() else {
        return false;
    };
    let sender_server = header.sender.server_name().to_string();
    let sender = header.sender.clone();
    let cited = hs_room::pipeline::decode_event_ids(event.json().get("auth_events"));
    let is_leave = membership == Some("leave");
    handle
        .query(move |actor| {
            if actor.local_user_joined() {
                return false;
            }
            let Some(prior) = actor.state_event("m.room.member", &target).ok().flatten() else {
                return false;
            };
            let known_server = actor
                .servers_to_join_through()
                .is_some_and(|servers| servers.contains(&sender_server));
            match content_str(prior, "membership") {
                // Only the inviter can rescind an invite this server cannot check the room's
                // power levels for, and only with a leave that cites the invite (Synapse's
                // `_process_received_pdu` rule): someone else in the room kicking the invitee
                // is not shown to them, as Complement's "Non-invitee user cannot rescind invite
                // over federation" expects.
                Some("invite") => {
                    is_leave
                        && prior.header().sender == sender
                        && cited.iter().any(|id| id == prior.event_id())
                        && known_server
                }
                Some("knock") => known_server,
                _ => false,
            }
        })
        .await
}

/// [`hs_federation::invite::InviteSink`] over `hs-room`'s [`RoomRegistry`]: an invite from
/// another server for one of this server's users is recorded in the room here
/// (`RoomRegistry::accept_out_of_room_membership`), with the stripped state the inviting server
/// sent kept on it as `unsigned.invite_room_state`, which is what the invitee's `/sync` shows.
///
/// If a user of this server is already in the room, nothing is recorded: the invite arrives
/// through the room itself, over `/send`, once the inviting server has put it in, the same as
/// any other event of a room this server is in.
pub struct RegistryInviteSink<B: KvBackend> {
    rooms: Arc<RoomRegistry<B>>,
}

impl<B: KvBackend + 'static> RegistryInviteSink<B> {
    /// Wraps an already-open registry.
    #[must_use]
    pub fn new(rooms: Arc<RoomRegistry<B>>) -> Self {
        Self { rooms }
    }
}

#[async_trait]
impl<B: KvBackend + 'static> hs_federation::invite::InviteSink for RegistryInviteSink<B> {
    async fn accept_invite(
        &self,
        room_version: &ruma::RoomVersionId,
        event: &hs_model::Event,
        invite_room_state: &[Value],
    ) -> Result<(), hs_federation::invite::InviteRejected> {
        use hs_federation::invite::InviteRejected;

        let room_id = event
            .json()
            .get("room_id")
            .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
            .and_then(|id| ruma::RoomId::parse(id).ok())
            .ok_or_else(|| InviteRejected("the invite names no room".to_owned()))?;
        if let Ok(handle) = self.rooms.get_or_load(&room_id).await
            && handle.query(|actor| actor.local_user_joined()).await
        {
            return Ok(());
        }
        let mut json = hs_federation::inbound::event_json(event);
        json["unsigned"]["invite_room_state"] = Value::Array(invite_room_state.to_vec());
        let event = hs_model::Event::parse(&json, room_version.clone())
            .map_err(|e| InviteRejected(format!("the invite does not parse: {e}")))?;
        self.rooms
            .accept_out_of_room_membership(&room_id, room_version.clone(), event)
            .await
            .map(|_| ())
            .map_err(|e| InviteRejected(e.to_string()))
    }
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
    /// What answers `/user/keys/query` and `/user/keys/claim`
    /// ([`ServerQuerySource::with_keys`]); `None` answers neither.
    keys: Option<hs_e2e::state::E2eState<B>>,
    /// Who is asked about a local alias the directory does not hold
    /// ([`ServerQuerySource::with_appservices`]); `None` asks nobody.
    appservices: Option<Arc<dyn hs_auth::appservice::AppserviceRegistry>>,
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
            keys: None,
            appservices: None,
        }
    }

    /// Asks the appservices whose alias namespace covers a local alias the directory does not
    /// hold, when another server queries it (`GET /query/directory`), as the client-server
    /// directory does (`hs_room::routes::aliases`) and as Synapse's
    /// `DirectoryHandler.get_association` does for both: a bridge that answers yes has created
    /// the room and the alias, and the answer is looked up again.
    #[must_use]
    pub fn with_appservices(
        mut self,
        appservices: Arc<dyn hs_auth::appservice::AppserviceRegistry>,
    ) -> Self {
        self.appservices = Some(appservices);
        self
    }

    /// Answers other servers' key queries and claims from `e2e`'s store
    /// (`hs_e2e::federation::federation_keys_query` and `federation_keys_claim`).
    #[must_use]
    pub fn with_keys(mut self, e2e: hs_e2e::state::E2eState<B>) -> Self {
        self.keys = Some(e2e);
        self
    }
}

#[async_trait]
impl<B: KvBackend + 'static> FederationQuerySource for ServerQuerySource<B> {
    async fn profile(&self, user_id: &str, field: Option<&str>) -> Option<Value> {
        let parsed = ruma::UserId::parse(user_id).ok()?;
        // An existing local user with nothing set gets an empty profile, not a 404: "this user
        // exists and has no display name" and "no such user" are different answers. A field
        // never set is left out, as the client-server `/profile` leaves it out.
        let record = self.auth.get_user(&parsed).await.ok().flatten()?;
        let mut profile = serde_json::Map::new();
        if field.is_none_or(|f| f == "displayname")
            && let Some(name) = record.display_name
        {
            profile.insert("displayname".to_owned(), Value::String(name));
        }
        if field.is_none_or(|f| f == "avatar_url")
            && let Some(avatar) = record.avatar_url
        {
            profile.insert("avatar_url".to_owned(), Value::String(avatar));
        }
        match field {
            Some("displayname" | "avatar_url") | None => Some(Value::Object(profile)),
            Some(_) => None,
        }
    }

    async fn resolve_alias(&self, alias: &str) -> Option<(String, Vec<String>)> {
        let parsed = ruma::RoomAliasId::parse(alias).ok()?;
        let room_id = match self.rooms.resolve_alias(&parsed).ok().flatten() {
            Some(room_id) => room_id,
            // Not held: a bridge whose namespace covers a local alias may provide it.
            None if parsed.server_name().as_str() == self.own_server_name => {
                let appservices = self.appservices.as_ref()?;
                if !appservices.query_room_alias(alias).await {
                    return None;
                }
                let provided = self.rooms.resolve_alias(&parsed).ok().flatten();
                tracing::info!(
                    alias,
                    provided = provided.is_some(),
                    "another server asked for a local alias an appservice provided"
                );
                provided?
            }
            None => return None,
        };
        // This server first, then every other server with a joined member, as Synapse answers:
        // what the asking server tries to join through.
        let mut servers = vec![self.own_server_name.clone()];
        if let Ok(handle) = self.rooms.get_or_load(&room_id).await {
            for server in handle.query(|actor| joined_servers(actor)).await {
                if !servers.contains(&server) {
                    servers.push(server);
                }
            }
        }
        Some((room_id.to_string(), servers))
    }

    async fn devices(&self, user_id: &str) -> Option<Value> {
        let parsed = ruma::UserId::parse(user_id).ok()?;
        // The whole list -- every device, named, with the cross-signing keys -- needs the
        // e2e state ([`ServerQuerySource::with_keys`]); without it, the devices with keys.
        if let Some(e2e) = self.keys.as_ref() {
            return match hs_e2e::federation::federation_user_devices(e2e, &parsed).await {
                Ok(answer) => answer,
                Err(error) => {
                    tracing::info!(user_id, %error, "could not answer a remote device-list query");
                    None
                }
            };
        }
        self.auth.get_user(&parsed).await.ok().flatten()?;
        let keys = self.e2e.list_device_keys(&parsed).await.ok()?;
        let stream_id = self.e2e.user_stream_pos(&parsed).await.ok()?;
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

    async fn openid_userinfo(&self, access_token: &str) -> Option<String> {
        // Tokens from `POST /user/{userId}/openid/request_token` (`hs_auth::openid`).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        match hs_auth::openid::userinfo(self.auth.as_ref(), access_token, now).await {
            Ok(user) => user.map(|u| u.to_string()),
            Err(error) => {
                tracing::warn!(%error, "could not look up an OpenID token");
                None
            }
        }
    }

    async fn keys_query(&self, origin: &str, device_keys: &Value) -> Option<Value> {
        let e2e = self.keys.as_ref()?;
        match hs_e2e::federation::federation_keys_query(e2e, device_keys).await {
            Ok(answer) => Some(answer),
            Err(error) => {
                tracing::info!(origin, %error, "could not answer a remote key query");
                None
            }
        }
    }

    async fn keys_claim(&self, origin: &str, one_time_keys: &Value) -> Option<Value> {
        let e2e = self.keys.as_ref()?;
        match hs_e2e::federation::federation_keys_claim(e2e, one_time_keys).await {
            Ok(answer) => Some(answer),
            Err(error) => {
                tracing::info!(origin, %error, "could not answer a remote key claim");
                None
            }
        }
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
    /// The outbound client, kept alive because the inbound key fetcher borrows it and the sender
    /// sends through it, sharing its backoff state.
    pub client: Arc<hs_federation::client::FederationClient>,
    /// The outbound sender: where this server's own events are queued for the servers of a
    /// room's remote members (`crate::federation_sender` feeds it), and where `send_join` hands
    /// a join it accepted so the room's other servers hear of it. Its queues are on the same
    /// backend as everything else (`hs_federation::outbound_store::KvOutboundStore`), so a
    /// restart resumes them; see `hs_federation::sender`'s module docs for what is still not
    /// caught up.
    pub sender: Arc<hs_federation::sender::FederationSender>,
    /// The per-destination backoff records the client keeps, for the admin API's Federation
    /// page (`hs_federation::admin_source`).
    pub destinations: Arc<dyn hs_federation::destination_store::DestinationStore>,
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
/// Returns the backend's error if the destination-backoff or outbound-queue keyspaces cannot be
/// opened.
// The stores this mount reads, plus the test seams (`scheme`, the resolvers). A struct
// of them would be built at exactly one call site and read at exactly one, which is the same
// list twice.
#[allow(clippy::too_many_arguments)]
pub fn build_mount<B: KvBackend + 'static>(
    config: &hs_config::Config,
    identity: &hs_room::identity::HomeserverIdentity,
    backend: B,
    rooms: Arc<RoomRegistry<B>>,
    auth: Arc<dyn hs_auth::store::AuthStore>,
    appservices: Arc<dyn hs_auth::appservice::AppserviceRegistry>,
    e2e: hs_e2e::state::E2eState<B>,
    scheme: Option<&'static str>,
    resolver_override: Option<crate::serve::FederationResolvers>,
) -> Result<FederationMount, hs_kv::KvError> {
    let server_name = identity.server_name.to_string();
    let own_keys = Arc::new(hs_federation::keys::OwnSigningKeys::from_keys(vec![
        (*identity.signing_key).clone(),
    ]));

    let destinations: Arc<dyn hs_federation::destination_store::DestinationStore> = Arc::new(
        hs_federation::destination_store::KvDestinationStore::open(backend.clone())?,
    );
    // The sender's queues, on the same backend as the events they carry: what is queued for a
    // destination that is down survives a restart of this process, and
    // `crate::federation_sender::OutboundFederation::start` resumes it.
    let outbound_store: Arc<dyn hs_federation::outbound_store::OutboundStore> = Arc::new(
        hs_federation::outbound_store::KvOutboundStore::open(backend.clone())?,
    );
    // Other servers' key responses, kept: the notary answers for a server that is down after a
    // restart, and keys verify without being fetched again (`hs_federation::key_store`).
    let held_keys: Arc<dyn hs_federation::key_store::HeldKeyStore> =
        Arc::new(hs_federation::key_store::KvHeldKeyStore::open(backend)?);
    let well_known = Arc::new(hs_federation::discovery::CachingWellKnownFetcher::new(
        hs_federation::discovery::HttpWellKnownFetcher::new(),
    ));
    // The system's resolvers, unless a test gave the server its own
    // (`ServeOptions::federation_resolvers`).
    let (srv, addr) = resolver_override.unwrap_or_else(resolvers);

    let client = Arc::new(hs_federation::client::FederationClient::new(
        server_name.clone(),
        (*identity.signing_key).clone(),
        client_config(&config.federation, scheme),
        destinations.clone(),
        well_known,
        srv,
        addr,
    ));

    let key_cache: Arc<hs_federation::keys::DynRemoteKeyCache> =
        Arc::new(hs_federation::keys::RemoteKeyCache::with_store(
            Box::new(ClientKeyFetcher::new(client.clone()))
                as Box<dyn hs_federation::keys::KeyServerFetcher>,
            held_keys,
        ));

    // The same client again: a transaction to a destination that is backing off waits for the
    // same `retry_at` every other outbound call to it does, and an administrator's reset of that
    // destination releases both.
    // In a cluster the store is shared and another replica may write rows for a destination
    // this one sends for, so an idle worker looks again every so often; alone, the store only
    // ever holds what this process's own channels already carried.
    let store_rescan_interval =
        (!config.cluster.single_node).then(|| std::time::Duration::from_secs(10));
    let sender = Arc::new(hs_federation::sender::FederationSender::with_store(
        client.clone(),
        server_name.clone(),
        hs_federation::sender::SenderConfig {
            store_rescan_interval,
            max_queued_pdus_per_destination: usize::try_from(
                config.federation.max_queued_pdus_per_destination,
            )
            .unwrap_or(usize::MAX),
            ..hs_federation::sender::SenderConfig::for_client(&client)
        },
        outbound_store,
    ));

    let state = hs_federation::transport::FederationState {
        own_server_name: Arc::from(server_name.as_str()),
        rooms: Arc::new(RegistryRoomSource::new(rooms.clone()).with_erasure(auth.clone())),
        queries: Arc::new(
            ServerQuerySource::new(auth, e2e.store.clone(), rooms.clone(), server_name.clone())
                .with_keys(e2e)
                .with_appservices(appservices),
        ),
        policy: hs_federation::transport::InboundPolicy::new(
            config.federation.allow_public_rooms_over_federation,
            config.federation.allow_device_name_lookup_over_federation,
        ),
        write_sink: Arc::new(RegistryWriteSink::new(rooms.clone())),
        transactions: Arc::new(hs_federation::inbound::InMemoryTransactionStore::new()),
        // The same client this mount uses for every other outbound call: `FederationClient`
        // implements `AncestorFetcher` directly (`crate::backfill`'s doc), so a missing-ancestor
        // gap is closed against the same signing key, backoff state and concurrency limits as any
        // other request to that destination.
        ancestor_fetcher: Some(client.clone() as Arc<dyn hs_federation::backfill::AncestorFetcher>),
        backfill_limits: hs_federation::backfill::BackfillLimits::default(),
        sender: Some(sender.clone() as Arc<dyn hs_federation::sender::OutboundPduSink>),
        // An invite from another server is co-signed with this server's event key and recorded
        // in the room here (`RegistryInviteSink`).
        invites: Some(hs_federation::invite::InviteHandling {
            sink: Arc::new(RegistryInviteSink::new(rooms)),
            signing_key: identity.signing_key.clone(),
        }),
        edu_sink: None,
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
        sender,
        destinations,
    })
}

/// `hs-config`'s federation settings, in the shape the outbound client takes them.
///
/// **Bug fixed here, found by actually running two live instances against each other
/// (`docs/status/06-federation.md`'s seventh session)**: this function used to build the outbound
/// client's `ClientConfig` with `..ClientConfig::default()` for everything it did not list
/// explicitly, which silently defaulted `custom_root_certificates` to empty and
/// `trust_os_root_store` to `false` regardless of what `federation.custom_ca_certificates` /
/// `federation.trust_os_root_store` said in the YAML. `hs-federation`'s own sixth session added
/// real config fields and a real `ClientConfig` seam for exactly this (see that struct's own doc
/// comments), and proved the seam works with an in-process test that constructs `ClientConfig`
/// directly -- but nothing ever read `custom_ca_certificates`' *paths* off disk and passed the
/// bytes through at the one site (`build_mount`, below) that actually boots a `FederationClient`
/// for `hs serve`. The result: setting `federation.custom_ca_certificates` in a real config file
/// had never had any effect on a running server, only in this crate's own unit tests -- the exact
/// "everything landed, nothing was put together" gap this session's brief warned about. Confirmed
/// by grep (`custom_root_certificates`/`custom_ca_certificates`/`trust_os_root_store` appeared
/// nowhere in `crates/hs-cli/src/*.rs` before this fix) and then by running it: two live `hs
/// serve` instances federating over TLS terminated with a private CA (see
/// `crates/hs-federation/scripts/two-server-federation.sh`) failed outbound TLS verification
/// until this function actually read the configured CA file.
///
/// A CA file that fails to read is logged loudly and skipped, not a fatal boot error: matching
/// `FederationClient::new`'s own tolerance for a CA entry that parses but is malformed, an entry
/// that cannot even be read (typo'd path, permissions) should be visible in the log the first time
/// this server tries to use it, not a mysterious refusal to start.
fn client_config(
    config: &hs_config::FederationConfig,
    scheme: Option<&'static str>,
) -> hs_federation::client::ClientConfig {
    let custom_root_certificates = config
        .custom_ca_certificates
        .iter()
        .filter_map(|path| match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                tracing::error!(
                    path,
                    %error,
                    "federation.custom_ca_certificates entry could not be read and will NOT be \
                     trusted for outbound federation TLS"
                );
                None
            }
        })
        .collect();

    hs_federation::client::ClientConfig {
        enabled: config.enabled,
        domain_policy: hs_federation::client::DomainPolicy::new(config.domain_allowlist.clone()),
        ip_policy: hs_federation::client::IpPolicy::from_cidrs(
            &config.ip_range_blocklist,
            &config.ip_range_allowlist,
        ),
        verify_certificates: config.verify_certificates,
        custom_root_certificates,
        trust_os_root_store: config.trust_os_root_store,
        request_timeout: config.client_timeout.into(),
        max_retry_backoff: config.max_retry_backoff.into(),
        scheme: scheme.unwrap_or(hs_federation::client::ClientConfig::default().scheme),
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

/// What the key server (`hs_federation::transport::key_server`) answers from: this server's
/// name and keys, [`KEY_VALIDITY_SECS`], no `old_verify_keys` (nothing rotates keys yet), and
/// `cache`, the remote-key cache inbound `X-Matrix` verification fills, which the notary
/// `/_matrix/key/v2/query` answers from.
#[must_use]
pub fn key_server_state(
    server_name: &str,
    own_keys: Arc<hs_federation::keys::OwnSigningKeys>,
    cache: Arc<hs_federation::keys::DynRemoteKeyCache>,
) -> hs_federation::transport::key_server::KeyServerState {
    hs_federation::transport::key_server::KeyServerState {
        server_name: Arc::from(server_name),
        own_keys,
        old_keys: Arc::from(Vec::new()),
        valid_for_secs: KEY_VALIDITY_SECS,
        cache,
    }
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
        policy: hs_federation::transport::InboundPolicy::new(false, false),
        write_sink: Arc::new(hs_federation::inbound::StaticWriteSink::new(
            Vec::new(),
            "manifest-only mount",
        )),
        transactions: Arc::new(hs_federation::inbound::InMemoryTransactionStore::new()),
        ancestor_fetcher: None,
        backfill_limits: hs_federation::backfill::BackfillLimits::default(),
        sender: None,
        invites: None,
        edu_sink: None,
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

// ------------------------------------------------------------------------------------------
// `hs federation-join-room`: the client-role join handshake, driven from the command line
// ------------------------------------------------------------------------------------------

/// Implements `hs federation-join-room`: loads `config_path` the same way `hs serve` would (same
/// server name, same signing key, same federation policy -- TLS/CA trust, IP range policy,
/// timeouts), then performs a real `hs_federation::outbound_join::join_room` against
/// `args.destination`, printing what was verified.
///
/// Deliberately opens no storage: everything this needs (`server_name`, the signing key,
/// `hs-config::FederationConfig`) comes from the config file alone, and nothing it produces can be
/// persisted locally yet (see `hs_federation::outbound_join`'s module doc and
/// `docs/rfcs/0015-outbound-join-needs-a-room-bootstrap-api.md` for why) -- so there is no local
/// room store for it to open in the first place. Uses an in-memory destination-backoff store
/// (`InMemoryDestinationStore`) rather than `KvDestinationStore`, unlike `build_mount`'s `hs
/// serve` client, for the same reason: a one-shot command has no state worth persisting across
/// runs.
pub async fn run_join_room(args: &crate::cli::FederationJoinRoomArgs) -> i32 {
    let config = match hs_config::Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hs federation-join-room: {e}");
            return 1;
        }
    };
    let identity = match crate::identity::load_or_generate(&config) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("hs federation-join-room: invalid server.server_name: {e}");
            return 1;
        }
    };

    let destinations = Arc::new(hs_federation::destination_store::InMemoryDestinationStore::new());
    let well_known = Arc::new(hs_federation::discovery::CachingWellKnownFetcher::new(
        hs_federation::discovery::HttpWellKnownFetcher::new(),
    ));
    let (srv, addr) = resolvers();
    let client = Arc::new(hs_federation::client::FederationClient::new(
        identity.server_name.to_string(),
        (*identity.signing_key).clone(),
        client_config(&config.federation, None),
        destinations,
        well_known,
        srv,
        addr,
    ));
    let key_cache: hs_federation::keys::DynRemoteKeyCache =
        hs_federation::keys::RemoteKeyCache::new(Box::new(ClientKeyFetcher::new(client.clone()))
            as Box<dyn hs_federation::keys::KeyServerFetcher>);

    match hs_federation::outbound_join::join_room(
        &client,
        &key_cache,
        &args.destination,
        &args.room,
        &args.user,
        &identity.server_name,
        &identity.signing_key,
    )
    .await
    {
        Ok(outcome) => {
            println!("join accepted by {}", args.destination);
            println!("  room_id:      {}", outcome.room_id);
            println!("  room_version: {}", outcome.room_version.as_str());
            println!("  join_event:   {}", outcome.join_event.event_id());
            println!("  state events verified:      {}", outcome.state.len());
            println!("  auth_chain events verified: {}", outcome.auth_chain.len());
            println!("  members_omitted: {}", outcome.members_omitted);
            println!();
            println!(
                "This join is real and durably persisted on {}'s side -- confirm with, e.g.,",
                args.destination
            );
            println!(
                "  GET /_matrix/client/v3/rooms/{}/members there. Nothing was stored here:",
                outcome.room_id
            );
            println!(
                "this command opens no storage. To join a room for {} and keep it, use the",
                args.user
            );
            println!(
                "client API of a running server: POST /_matrix/client/v3/join/{{roomId}}?server_name=..."
            );
            0
        }
        Err(e) => {
            eprintln!("hs federation-join-room: {e}");
            1
        }
    }
}
