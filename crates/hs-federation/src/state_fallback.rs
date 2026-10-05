//! The `/state_ids` fallback: a prev event this server could not walk back to, taken with the
//! state another server answers for it.
//!
//! [`crate::backfill::resolve_missing_ancestors`] closes a gap by fetching the missing history.
//! When the server that sent the event does not hand all of it over -- it answers
//! `/get_missing_events` with one hop and `/backfill` with nothing or `404` (Sytest's server),
//! the history is too long, or it was never there -- the events it did hand over cannot be
//! placed: their own prev events are still unknown. The spec's other way, and Synapse's
//! (`_compute_event_context_with_maybe_missing_prevs`), is to ask that server for the state at
//! each such prev event (`GET /state_ids`), fetch the events it names that this server lacks
//! (`GET /event` each, or all at once with `GET /state` when many are missing), and hold the
//! prev event as an outlier with that state ([`RoomWriteSink::accept_prev_event_with_state`]).
//! The fetched event is then accepted at the state resolved from its prev events, and the
//! event received over `/send` after it. `hs-room`'s `actor::fetched_state` is the room side.
//!
//! # Which events get this
//!
//! The ones backfill fetched and could not place (its `pending`), never the received event
//! itself. An event sent directly whose prev events its sender will not divulge is refused, as
//! Synapse refuses it ("Your server isn't divulging details about prev_events referenced in
//! this event") and as Sytest's "Federation rejects inbound events where the prev_events cannot
//! be found" asserts: this server must not ask for the state at *that* prev event. An event
//! pushed to us could otherwise become the room's only forward extremity with a state its
//! sender made up.
//!
//! # Order of requests
//!
//! `/state_ids` first, then `/event` for the prev event and what the state names: a server
//! that cannot answer the state at an event is asked `/state` once, and one that cannot
//! answer the event itself (Sytest's serves `/event` from its store, which an event it made
//! up for a test is not in) ends the fallback for that prev event with nothing taken.
//!
//! # What is checked here, and what the room checks
//!
//! Every fetched event is verified as any inbound PDU is ([`verify_pdu`]: hashes and
//! signatures), is checked to be the event asked for, and to be of the room (an event of
//! another room named in an `auth_chain_ids` -- Sytest's "outliers whose auth_events are in a
//! different room" -- is dropped, so the citing event's auth check lacks it and refuses it).
//! Authorisation is the room's: each fetched event against its own auth events, the prev event
//! against the fetched state, a refused one stored rejected and left out of the state. A server
//! answering a made-up state (Sytest's "Should not be able to take over the room by pretending
//! there is no PL event") gains nothing: its made-up power levels fail against the real ones.
//!
//! # Bounds
//!
//! At most [`MAX_PENDING_EVENTS`] pending events are tried, at most [`MAX_PREV_EVENTS`] missing
//! prev events per pending event, at most [`MAX_EVENT_FETCHES`] `/event` fetches per state
//! (beyond that, or when a tenth or more of what the state names is missing, `/state` is asked
//! once instead), one further round for the auth events of what was fetched, and the whole
//! fallback runs under [`BackfillLimits::max_duration`]. A hostile peer costs this server at
//! most that many signature checks per received event.

use std::collections::HashSet;

use futures::StreamExt as _;
use hs_model::Event;
use ruma::RoomVersionId;
use serde_json::Value;

use crate::backfill::{AncestorFetcher, BackfillLimits};
use crate::inbound::{RoomWriteSink, event_json, verify_pdu};
use crate::keys::DynRemoteKeyCache;
use crate::metrics::record_state_fallback;

/// How many of the events backfill left pending may cost a state fetch, oldest (lowest
/// `depth`) first.
pub const MAX_PENDING_EVENTS: usize = 5;
/// How many missing prev events of one pending event are taken with a state; one with more
/// is not tried at all.
pub const MAX_PREV_EVENTS: usize = 5;
/// How many events of one state are fetched one by one (`/event`); more than this, or a tenth
/// or more of what the state names, and the whole state is asked for once (`/state`).
pub const MAX_EVENT_FETCHES: usize = 100;
/// How many `/event` fetches are in flight at once.
const EVENT_FETCH_CONCURRENCY: usize = 4;

/// Why [`resolve_through_state`] could not take any of the events it was given.
#[derive(Debug, Clone)]
pub struct StateFallbackError(pub String);

impl std::fmt::Display for StateFallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StateFallbackError {}

/// Takes each of `pending` -- events fetched from `origin` for `room_id` and verified, which the
/// sink could not place because their own prev events are unknown -- by asking `origin` for
/// the state at each such prev event and holding it with that state through `sink`, then
/// offering the pending event to the sink again. See the module docs for the order of
/// requests, the checks and the bounds.
///
/// `Ok(())` when at least one pending event was placed (or processed and rejected): the
/// caller's retry of the event that started all this is worth making. The events are tried
/// oldest first, so a chain of pending events is placed in order.
///
/// # Errors
/// [`StateFallbackError`] when none could be: the first failure's reason, or the time limit.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_through_state(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    pending: Vec<Event>,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
    sink: &dyn RoomWriteSink,
    limits: &BackfillLimits,
) -> Result<(), StateFallbackError> {
    match tokio::time::timeout(
        limits.max_duration,
        resolve_all(
            origin,
            room_id,
            room_version,
            pending,
            fetcher,
            key_cache,
            sink,
        ),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            record_state_fallback("timed_out");
            tracing::warn!(
                origin,
                room_id,
                "the /state_ids fallback timed out before any missing prev event was taken"
            );
            Err(StateFallbackError("timed out".to_owned()))
        }
    }
}

async fn resolve_all(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    mut pending: Vec<Event>,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
    sink: &dyn RoomWriteSink,
) -> Result<(), StateFallbackError> {
    // Oldest first: once the oldest of a chain is placed through a state, the next one's prev
    // event is held and it is placed as it is, costing no state fetch.
    pending.sort_by_key(|event| event.header().depth);
    let mut placed = 0usize;
    let mut states_tried = 0usize;
    let mut first_failure: Option<String> = None;
    for event in &pending {
        let event_id = event.event_id().to_string();
        match sink
            .accept_verified_event(room_id, &event_id, &event_json(event))
            .await
        {
            Ok(_) => {
                placed += 1;
                continue;
            }
            Err(rejected) if rejected.auth_rejected => {
                placed += 1;
                continue;
            }
            Err(rejected) if rejected.missing_ancestors.is_empty() => {
                first_failure.get_or_insert(rejected.error);
                continue;
            }
            Err(_) => {}
        }
        if states_tried >= MAX_PENDING_EVENTS {
            first_failure.get_or_insert_with(|| {
                format!(
                    "more than {MAX_PENDING_EVENTS} fetched events need the state at a missing prev event"
                )
            });
            continue;
        }
        states_tried += 1;
        match resolve_one(
            origin,
            room_id,
            room_version,
            event,
            fetcher,
            key_cache,
            sink,
        )
        .await
        {
            Ok(()) => placed += 1,
            Err(StateFallbackError(reason)) => {
                tracing::info!(
                    origin,
                    room_id,
                    %event_id,
                    %reason,
                    "a fetched event could not be placed through the state at its missing prev events"
                );
                first_failure.get_or_insert(reason);
            }
        }
    }
    if placed > 0 {
        return Ok(());
    }
    Err(StateFallbackError(first_failure.unwrap_or_else(|| {
        "no fetched event was pending".to_owned()
    })))
}

/// One pending event: the state at each of its missing prev events, then the event again.
async fn resolve_one(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    event: &Event,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
    sink: &dyn RoomWriteSink,
) -> Result<(), StateFallbackError> {
    let event_id = event.event_id().to_string();
    let prev_ids = ids_named(event, "prev_events");
    let auth_ids = ids_named(event, "auth_events");
    let missing_prevs = sink.unknown_events(room_id, &prev_ids).await;
    if missing_prevs.len() > MAX_PREV_EVENTS {
        return Err(StateFallbackError(format!(
            "{event_id} cites {} unknown prev events, more than the {MAX_PREV_EVENTS} this server asks the state at",
            missing_prevs.len()
        )));
    }
    // The pending event's own missing auth events come along with the first state fetched: a
    // state's auth chain names them when the state is honest, and asking for them by ID costs
    // nothing when it is not.
    let missing_auth = sink.unknown_events(room_id, &auth_ids).await;
    for (index, prev_id) in missing_prevs.iter().enumerate() {
        let extra: &[String] = if index == 0 { &missing_auth } else { &[] };
        take_prev_event_with_state(
            origin,
            room_id,
            room_version,
            prev_id,
            extra,
            fetcher,
            key_cache,
            sink,
        )
        .await?;
    }
    match sink
        .accept_verified_event(room_id, &event_id, &event_json(event))
        .await
    {
        Ok(_) => Ok(()),
        // Processed and refused: the sink stored it rejected, and an event citing it is
        // judged at the state before it. That is progress, not a failure of the fallback.
        Err(rejected) if rejected.auth_rejected => Ok(()),
        Err(rejected) => Err(StateFallbackError(format!(
            "{event_id} still cannot be placed: {}",
            rejected.error
        ))),
    }
}

/// `prev_id`, as `origin` answers it, held with the state before it as `origin` answers that:
/// see the module docs for the order and the bounds. `extra` are further events to fetch along
/// with the state (the citing event's own missing auth events).
#[allow(clippy::too_many_arguments)]
async fn take_prev_event_with_state(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    prev_id: &str,
    extra: &[String],
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
    sink: &dyn RoomWriteSink,
) -> Result<(), StateFallbackError> {
    // 1. The state at the prev event, by ID when the server answers that, else whole.
    let state = match fetcher.fetch_state_ids(origin, room_id, prev_id).await {
        Ok((pdu_ids, auth_chain_ids)) => FetchedState {
            state_ids: dedup(pdu_ids),
            auth_chain_ids: dedup(auth_chain_ids),
            events: Vec::new(),
        },
        Err(error) => {
            tracing::debug!(origin, room_id, event_id = prev_id, %error, "/state_ids was not answered; asking /state");
            match fetch_whole_state(origin, room_id, room_version, prev_id, fetcher, key_cache)
                .await
            {
                Some(state) => state,
                None => {
                    record_state_fallback("no_state");
                    return Err(StateFallbackError(format!(
                        "{origin} answered neither /state_ids nor /state at {prev_id}: {error}"
                    )));
                }
            }
        }
    };

    // 2. The prev event itself.
    let Some(prev) =
        fetch_verified_event(origin, room_id, room_version, prev_id, fetcher, key_cache).await
    else {
        record_state_fallback("no_event");
        return Err(StateFallbackError(format!(
            "{origin} did not answer /event for the missing prev event {prev_id}, or what it answered did not verify"
        )));
    };

    // 3. What the state names, its auth chain, the prev event's own auth events and `extra`,
    //    less what is held: one by one, or the whole state when much is missing.
    let FetchedState {
        state_ids,
        auth_chain_ids,
        mut events,
    } = state;
    let named = state_ids.len() + auth_chain_ids.len();
    let mut have: HashSet<String> = events.iter().map(|e| e.event_id().to_string()).collect();
    have.insert(prev_id.to_owned());
    let wanted: Vec<String> = dedup(
        state_ids
            .iter()
            .chain(auth_chain_ids.iter())
            .chain(ids_named(&prev, "auth_events").iter())
            .chain(extra.iter())
            .filter(|id| !have.contains(*id))
            .cloned()
            .collect(),
    );
    let mut missing = sink.unknown_events(room_id, &wanted).await;
    if events.is_empty()
        && (missing.len() > MAX_EVENT_FETCHES
            || (!missing.is_empty() && missing.len() * 10 >= named))
    {
        tracing::debug!(
            origin,
            room_id,
            event_id = prev_id,
            missing = missing.len(),
            named,
            "fetching the whole state at once"
        );
        if let Some(whole) =
            fetch_whole_state(origin, room_id, room_version, prev_id, fetcher, key_cache).await
        {
            for event in whole.events {
                if have.insert(event.event_id().to_string()) {
                    events.push(event);
                }
            }
            missing.retain(|id| !have.contains(id));
        }
    }
    let fetched = fetch_events(origin, room_id, room_version, missing, fetcher, key_cache).await;
    for event in fetched {
        if have.insert(event.event_id().to_string()) {
            events.push(event);
        }
    }

    // 4. One more round, for the auth events of what was fetched: an outlier is judged by its
    //    own auth events, which an honest auth chain already names.
    let more: Vec<String> = dedup(
        events
            .iter()
            .flat_map(|event| ids_named(event, "auth_events"))
            .filter(|id| !have.contains(id))
            .collect(),
    );
    let more_missing = sink.unknown_events(room_id, &more).await;
    let fetched = fetch_events(
        origin,
        room_id,
        room_version,
        more_missing,
        fetcher,
        key_cache,
    )
    .await;
    for event in fetched {
        if have.insert(event.event_id().to_string()) {
            events.push(event);
        }
    }

    // 5. The room holds it with that state.
    let fetched_json: Vec<Value> = events.iter().map(event_json).collect();
    let fetched_count = events.len();
    match sink
        .accept_prev_event_with_state(
            room_id,
            prev_id,
            &event_json(&prev),
            &state_ids,
            &fetched_json,
        )
        .await
    {
        Ok(_) => {
            record_state_fallback("resolved");
            tracing::info!(
                origin,
                room_id,
                event_id = prev_id,
                state_events = state_ids.len(),
                fetched = fetched_count,
                "took a missing prev event with the state another server answered for it"
            );
            Ok(())
        }
        Err(rejected) if rejected.auth_rejected => {
            record_state_fallback("rejected");
            tracing::info!(
                origin,
                room_id,
                event_id = prev_id,
                fetched = fetched_count,
                reason = %rejected.error,
                "a missing prev event was fetched with its state and authorisation refused it; stored rejected"
            );
            Ok(())
        }
        Err(rejected) => {
            record_state_fallback("refused");
            Err(StateFallbackError(format!(
                "the room could not hold {prev_id} with its fetched state: {}",
                rejected.error
            )))
        }
    }
}

/// A state as fetched: the IDs of its events and auth chain, and the events already in hand
/// (every one of them when it came from `/state`, none from `/state_ids`).
struct FetchedState {
    state_ids: Vec<String>,
    auth_chain_ids: Vec<String>,
    events: Vec<Event>,
}

/// `GET /state` at `event_id`: every state event and its auth chain, verified and of the room.
async fn fetch_whole_state(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    event_id: &str,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
) -> Option<FetchedState> {
    let (pdus, auth_chain) = match fetcher.fetch_state(origin, room_id, event_id).await {
        Ok(answer) => answer,
        Err(error) => {
            tracing::debug!(origin, room_id, event_id, %error, "/state was not answered");
            return None;
        }
    };
    let mut state = FetchedState {
        state_ids: Vec::with_capacity(pdus.len()),
        auth_chain_ids: Vec::with_capacity(auth_chain.len()),
        events: Vec::with_capacity(pdus.len() + auth_chain.len()),
    };
    let mut dropped = 0usize;
    for (raw, in_state) in pdus
        .iter()
        .map(|raw| (raw, true))
        .chain(auth_chain.iter().map(|raw| (raw, false)))
    {
        match verified_of_room(raw, room_id, room_version, key_cache).await {
            Some(event) => {
                let id = event.event_id().to_string();
                if in_state {
                    state.state_ids.push(id);
                } else {
                    state.auth_chain_ids.push(id);
                }
                state.events.push(event);
            }
            None => dropped += 1,
        }
    }
    if dropped > 0 {
        tracing::warn!(
            origin,
            room_id,
            event_id,
            dropped,
            "events of a fetched state did not verify or were of another room; the state lacks them"
        );
    }
    Some(state)
}

/// How many rounds [`fetch_missing_auth_events`] walks back through auth events of auth events.
const MAX_AUTH_ROUNDS: usize = 10;

/// Fetches the auth events a received event cites and this server lacks -- `missing`, from
/// `origin`, one `GET /event` each -- and theirs in turn that it lacks too (at most
/// [`MAX_AUTH_ROUNDS`] rounds, [`MAX_EVENT_FETCHES`] events a round), verified and of the room,
/// and hands them all to `sink` as outliers ([`RoomWriteSink::accept_auth_outliers`]), which
/// judges each by its own auth events. Called by `crate::inbound::process_transaction` for an
/// event whose prev events are all held; its caller then offers the event again.
///
/// # Errors
/// A description, when nothing could be fetched or the sink would not hold what was.
pub async fn fetch_missing_auth_events(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    missing: Vec<String>,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
    sink: &dyn RoomWriteSink,
) -> Result<(), String> {
    let mut wanted = dedup(missing);
    let mut asked: HashSet<String> = HashSet::new();
    let mut fetched: Vec<Event> = Vec::new();
    for _ in 0..MAX_AUTH_ROUNDS {
        wanted.retain(|id| asked.insert(id.clone()));
        if wanted.is_empty() {
            break;
        }
        let batch = fetch_events(origin, room_id, room_version, wanted, fetcher, key_cache).await;
        let cited: Vec<String> = dedup(
            batch
                .iter()
                .flat_map(|event| ids_named(event, "auth_events"))
                .filter(|id| !asked.contains(id))
                .collect(),
        );
        fetched.extend(batch);
        wanted = sink.unknown_events(room_id, &cited).await;
    }
    if fetched.is_empty() {
        return Err("none of the missing auth events could be fetched".to_owned());
    }
    let events: Vec<Value> = fetched.iter().map(event_json).collect();
    match sink.accept_auth_outliers(room_id, &events).await {
        Ok(held) => {
            tracing::info!(
                origin,
                room_id,
                fetched = fetched.len(),
                held,
                "fetched the auth events a received event cites and this server lacked"
            );
            Ok(())
        }
        Err(rejected) => Err(rejected.error),
    }
}

/// `GET /event` for one ID: verified, of the room, and the event asked for.
async fn fetch_verified_event(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    event_id: &str,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
) -> Option<Event> {
    let raw = match fetcher.fetch_event(origin, event_id).await {
        Ok(raw) => raw,
        Err(error) => {
            tracing::debug!(origin, room_id, event_id, %error, "/event was not answered");
            return None;
        }
    };
    let event = verified_of_room(&raw, room_id, room_version, key_cache).await?;
    if event.event_id() != event_id {
        tracing::warn!(origin, asked = event_id, got = %event.event_id(), "a server answered /event with another event");
        return None;
    }
    Some(event)
}

/// `GET /event` for each of `ids`, at most [`MAX_EVENT_FETCHES`], a few at a time; what fails
/// is left out. The order of the result is not the order of `ids`.
async fn fetch_events(
    origin: &str,
    room_id: &str,
    room_version: &RoomVersionId,
    ids: Vec<String>,
    fetcher: &dyn AncestorFetcher,
    key_cache: &DynRemoteKeyCache,
) -> Vec<Event> {
    let asked = ids.len().min(MAX_EVENT_FETCHES);
    let events: Vec<Event> = futures::stream::iter(ids.into_iter().take(MAX_EVENT_FETCHES))
        .map(|id| async move {
            fetch_verified_event(origin, room_id, room_version, &id, fetcher, key_cache).await
        })
        .buffer_unordered(EVENT_FETCH_CONCURRENCY)
        .filter_map(|event| async move { event })
        .collect()
        .await;
    if events.len() < asked {
        tracing::warn!(
            origin,
            room_id,
            failed = asked - events.len(),
            fetched = events.len(),
            "events of a fetched state could not be had; the state lacks them"
        );
    }
    events
}

/// `raw` verified ([`verify_pdu`]) and of `room_id`.
async fn verified_of_room(
    raw: &Value,
    room_id: &str,
    room_version: &RoomVersionId,
    key_cache: &DynRemoteKeyCache,
) -> Option<Event> {
    let event = match verify_pdu(raw, room_version, key_cache).await {
        Ok(event) => event,
        Err(error) => {
            // Room versions 1 and 2 carry the ID; later ones derive it from what failed.
            let event_id = raw.get("event_id").and_then(Value::as_str).unwrap_or("");
            let event_type = raw.get("type").and_then(Value::as_str).unwrap_or("");
            tracing::debug!(room_id, event_id, event_type, %error, "a fetched event does not verify");
            return None;
        }
    };
    let of_room = event
        .json()
        .get("room_id")
        .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
        == Some(room_id);
    if !of_room {
        tracing::info!(room_id, event_id = %event.event_id(), "a fetched event is of another room; dropped");
        return None;
    }
    Some(event)
}

/// The event IDs `event`'s `field` (`prev_events` or `auth_events`) names, in the event's
/// own form for its room version.
fn ids_named(event: &Event, field: &str) -> Vec<String> {
    let Some(value) = event.json().get(field) else {
        return Vec::new();
    };
    let json: Value = serde_json::from_slice(&value.to_canonical_bytes()).unwrap_or(Value::Null);
    let Some(items) = json.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            // Room versions 1 and 2: `[event_id, hashes]`.
            Value::Array(pair) => pair.first().and_then(Value::as_str),
            other => other.as_str(),
        })
        .map(str::to_owned)
        .collect()
}

/// `ids` without duplicates, first occurrence kept.
fn dedup(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::backfill::tests::{key_cache, signed_message};
    use crate::backfill::{
        AncestorFetchError, BackfillGiveUpReason, GapContext, resolve_missing_ancestors,
    };
    use crate::inbound::{WriteOutcome, WriteRejected};
    use crate::keys::OwnSigningKeys;

    const ORIGIN: &str = "origin.example.org";
    const ROOM: &str = "!r:origin.example.org";
    const SENDER: &str = "@alice:origin.example.org";

    fn id_of(pdu: &Value) -> String {
        Event::parse(pdu, RoomVersionId::V11)
            .unwrap()
            .event_id()
            .to_string()
    }

    /// Sytest's server, as its `/state_ids` tests stand it up: `/get_missing_events` answers
    /// one hop, `/backfill` is `404`, `/state_ids` at one event is answered, `/event` serves
    /// what it holds.
    struct SytestLikeFetcher {
        missing_events: Vec<Value>,
        state_ids_at: Option<(String, Vec<String>, Vec<String>)>,
        events: HashMap<String, Value>,
        state_ids_asked: Mutex<Vec<String>>,
        events_asked: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AncestorFetcher for SytestLikeFetcher {
        async fn fetch_backfill(
            &self,
            _destination: &str,
            _room_id: &str,
            _from_event_ids: &[String],
            _limit: usize,
        ) -> Result<Vec<Value>, AncestorFetchError> {
            Err(AncestorFetchError("HTTP 404".to_owned()))
        }

        async fn fetch_missing_events(
            &self,
            _destination: &str,
            _room_id: &str,
            _earliest_events: &[String],
            _latest_events: &[String],
            _limit: usize,
            _min_depth: i64,
        ) -> Result<Vec<Value>, AncestorFetchError> {
            Ok(self.missing_events.clone())
        }

        async fn fetch_state_ids(
            &self,
            _destination: &str,
            _room_id: &str,
            event_id: &str,
        ) -> Result<(Vec<String>, Vec<String>), AncestorFetchError> {
            self.state_ids_asked
                .lock()
                .unwrap()
                .push(event_id.to_owned());
            match &self.state_ids_at {
                Some((at, state, chain)) if at == event_id => Ok((state.clone(), chain.clone())),
                _ => Err(AncestorFetchError("HTTP 404".to_owned())),
            }
        }

        async fn fetch_event(
            &self,
            _destination: &str,
            event_id: &str,
        ) -> Result<Value, AncestorFetchError> {
            self.events_asked.lock().unwrap().push(event_id.to_owned());
            self.events
                .get(event_id)
                .cloned()
                .ok_or_else(|| AncestorFetchError("HTTP 404".to_owned()))
        }
    }

    /// `(prev event, the state it was held with, the events fetched for it)`.
    type HeldWithState = (String, Vec<String>, Vec<String>);

    /// A room that places an event once its prev events are held, and can hold a prev event
    /// with a fetched state (recording what it was given).
    struct StateSink {
        known: Mutex<HashSet<String>>,
        held_with_state: Mutex<Vec<HeldWithState>>,
    }

    impl StateSink {
        fn new(known: impl IntoIterator<Item = String>) -> Self {
            Self {
                known: Mutex::new(known.into_iter().collect()),
                held_with_state: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl RoomWriteSink for StateSink {
        async fn accept_verified_event(
            &self,
            _room_id: &str,
            event_id: &str,
            event_json: &Value,
        ) -> Result<WriteOutcome, WriteRejected> {
            let mut known = self.known.lock().unwrap();
            if known.contains(event_id) {
                return Ok(WriteOutcome::AlreadyKnown);
            }
            let event = Event::parse(event_json, RoomVersionId::V11).unwrap();
            let missing: Vec<String> = ids_named(&event, "prev_events")
                .into_iter()
                .filter(|p| !known.contains(p))
                .collect();
            if !missing.is_empty() {
                return Err(WriteRejected::missing_ancestors(
                    missing,
                    "missing ancestor",
                ));
            }
            known.insert(event_id.to_owned());
            Ok(WriteOutcome::Stored)
        }

        async fn unknown_events(&self, _room_id: &str, event_ids: &[String]) -> Vec<String> {
            let known = self.known.lock().unwrap();
            event_ids
                .iter()
                .filter(|id| !known.contains(*id))
                .cloned()
                .collect()
        }

        async fn accept_prev_event_with_state(
            &self,
            _room_id: &str,
            prev_event_id: &str,
            _prev_event: &Value,
            state_before: &[String],
            fetched: &[Value],
        ) -> Result<WriteOutcome, WriteRejected> {
            let mut known = self.known.lock().unwrap();
            let fetched_ids: Vec<String> = fetched.iter().map(id_of).collect();
            known.extend(fetched_ids.iter().cloned());
            known.insert(prev_event_id.to_owned());
            self.held_with_state.lock().unwrap().push((
                prev_event_id.to_owned(),
                state_before.to_vec(),
                fetched_ids,
            ));
            Ok(WriteOutcome::Stored)
        }
    }

    /// Sytest's "Outbound federation requests missing prev_events and then asks for /state_ids
    /// and resolves the state": C cites X, `/get_missing_events` answers X, which cites Y;
    /// `/backfill` is `404`. The state at Y is asked for, Y and what the state names that this
    /// server lacks are fetched, Y is held with the state, X is placed, and the caller may
    /// retry C.
    #[tokio::test]
    async fn a_fetched_event_whose_prev_event_is_missing_is_placed_through_the_state_at_it() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, ORIGIN);
        let root = "$root".to_owned();
        let y = signed_message(&keys, ROOM, SENDER, vec![root.clone()], 1);
        let y_id = id_of(&y);
        let x = signed_message(&keys, ROOM, SENDER, vec![y_id.clone()], 2);
        let x_id = id_of(&x);
        let c = signed_message(&keys, ROOM, SENDER, vec![x_id.clone()], 3);
        let c_id = id_of(&c);
        // The state at Y: two events this server holds, one it does not (S), and an auth chain
        // naming one more it does not hold (A).
        let s = signed_message(&keys, ROOM, SENDER, vec![], -1);
        let s_id = id_of(&s);
        let a = signed_message(&keys, ROOM, SENDER, vec![], -2);
        let a_id = id_of(&a);
        let held_state = ["$create".to_owned(), "$power".to_owned()];
        let fetcher = SytestLikeFetcher {
            missing_events: vec![x.clone()],
            state_ids_at: Some((
                y_id.clone(),
                vec![held_state[0].clone(), held_state[1].clone(), s_id.clone()],
                vec![a_id.clone()],
            )),
            events: [(y_id.clone(), y), (s_id.clone(), s), (a_id.clone(), a)]
                .into_iter()
                .collect(),
            state_ids_asked: Mutex::new(Vec::new()),
            events_asked: Mutex::new(Vec::new()),
        };
        let sink = StateSink::new([root, held_state[0].clone(), held_state[1].clone()]);
        let before = crate::metrics::state_fallbacks("resolved");

        let outcome = resolve_missing_ancestors(
            ORIGIN,
            ROOM,
            &RoomVersionId::V11,
            vec![x_id.clone()],
            &GapContext {
                latest_event_id: Some(c_id),
                earliest_events: Vec::new(),
            },
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        assert!(outcome.is_ok(), "{outcome:?}");

        assert_eq!(*fetcher.state_ids_asked.lock().unwrap(), vec![y_id.clone()]);
        let asked = fetcher.events_asked.lock().unwrap().clone();
        assert!(
            asked.contains(&y_id) && asked.contains(&s_id) && asked.contains(&a_id),
            "{asked:?}"
        );
        assert!(
            !asked.iter().any(|id| held_state.contains(id)),
            "held events are not fetched: {asked:?}"
        );
        let held = sink.held_with_state.lock().unwrap().clone();
        assert_eq!(held.len(), 1);
        let (prev, state, fetched) = &held[0];
        assert_eq!(prev, &y_id);
        assert_eq!(
            state,
            &[held_state[0].clone(), held_state[1].clone(), s_id.clone()]
        );
        let fetched: HashSet<&String> = fetched.iter().collect();
        assert!(
            fetched.contains(&s_id) && fetched.contains(&a_id),
            "{fetched:?}"
        );
        assert!(
            sink.known.lock().unwrap().contains(&x_id),
            "X is placed after Y is held"
        );
        assert!(crate::metrics::state_fallbacks("resolved") > before);
    }

    /// Sytest's "Federation rejects inbound events where the prev_events cannot be found": the
    /// received event's own missing prev event, which `/get_missing_events` will not divulge,
    /// never gets the state asked at it.
    #[tokio::test]
    async fn the_received_events_own_missing_prev_event_never_gets_the_state_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, ORIGIN);
        let fetcher = SytestLikeFetcher {
            missing_events: Vec::new(),
            state_ids_at: None,
            events: HashMap::new(),
            state_ids_asked: Mutex::new(Vec::new()),
            events_asked: Mutex::new(Vec::new()),
        };
        let sink = StateSink::new(["$root".to_owned()]);
        let outcome = resolve_missing_ancestors(
            ORIGIN,
            ROOM,
            &RoomVersionId::V11,
            vec!["$missing".to_owned()],
            &GapContext {
                latest_event_id: Some("$sent".to_owned()),
                earliest_events: vec!["$root".to_owned()],
            },
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        assert!(
            matches!(outcome, Err(BackfillGiveUpReason::StillMissing(_))),
            "{outcome:?}"
        );
        assert!(fetcher.state_ids_asked.lock().unwrap().is_empty());
        assert!(sink.held_with_state.lock().unwrap().is_empty());
    }

    /// The state is asked for before the prev event is fetched (Sytest's "Should not be able
    /// to take over the room" waits for the `/state_ids` request and never serves the event);
    /// a prev event that cannot be fetched ends the fallback with nothing held, and an event
    /// of another room named by the state is dropped before the room sees it.
    #[tokio::test]
    async fn a_prev_event_that_cannot_be_fetched_fails_and_an_event_of_another_room_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let keys = OwnSigningKeys::load_or_generate(dir.path()).unwrap();
        let cache = key_cache(&keys, ORIGIN);
        let root = "$root".to_owned();
        let y = signed_message(&keys, ROOM, SENDER, vec![root.clone()], 1);
        let y_id = id_of(&y);
        let x = signed_message(&keys, ROOM, SENDER, vec![y_id.clone()], 2);
        let x_id = id_of(&x);
        let elsewhere = signed_message(&keys, "!other:origin.example.org", SENDER, vec![], -1);
        let elsewhere_id = id_of(&elsewhere);

        // Y is not served.
        let fetcher = SytestLikeFetcher {
            missing_events: vec![x.clone()],
            state_ids_at: Some((y_id.clone(), vec![root.clone()], vec![elsewhere_id.clone()])),
            events: [(elsewhere_id.clone(), elsewhere.clone())]
                .into_iter()
                .collect(),
            state_ids_asked: Mutex::new(Vec::new()),
            events_asked: Mutex::new(Vec::new()),
        };
        let sink = StateSink::new([root.clone()]);
        let before = crate::metrics::state_fallbacks("no_event");
        let outcome = resolve_missing_ancestors(
            ORIGIN,
            ROOM,
            &RoomVersionId::V11,
            vec![x_id.clone()],
            &GapContext {
                latest_event_id: Some("$c".to_owned()),
                earliest_events: vec![root.clone()],
            },
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        match outcome {
            Err(BackfillGiveUpReason::StateFallbackFailed { state, .. }) => {
                assert!(state.contains("did not answer /event"), "{state}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(*fetcher.state_ids_asked.lock().unwrap(), vec![y_id.clone()]);
        assert!(sink.held_with_state.lock().unwrap().is_empty());
        assert!(!sink.known.lock().unwrap().contains(&x_id));
        assert!(crate::metrics::state_fallbacks("no_event") > before);

        // Y served now: the event of another room is still not handed to the room.
        let fetcher = SytestLikeFetcher {
            events: [(elsewhere_id.clone(), elsewhere), (y_id.clone(), y)]
                .into_iter()
                .collect(),
            state_ids_asked: Mutex::new(Vec::new()),
            events_asked: Mutex::new(Vec::new()),
            ..fetcher
        };
        let outcome = resolve_missing_ancestors(
            ORIGIN,
            ROOM,
            &RoomVersionId::V11,
            vec![x_id.clone()],
            &GapContext {
                latest_event_id: Some("$c".to_owned()),
                earliest_events: vec![root],
            },
            &fetcher,
            &cache,
            &sink,
            &BackfillLimits::default(),
        )
        .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        let held = sink.held_with_state.lock().unwrap().clone();
        assert_eq!(held.len(), 1);
        assert!(!held[0].2.contains(&elsewhere_id), "{:?}", held[0].2);
        assert!(sink.known.lock().unwrap().contains(&x_id));
    }
}
