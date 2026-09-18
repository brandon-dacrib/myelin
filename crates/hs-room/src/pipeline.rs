//! The event creation pipeline: PLAN.md section 5.3's "event creation, authorization against
//! current state" -- everything between "a client asked to send this content" and "here is a
//! signed, authorized [`Event`] ready to persist".
//!
//! # Steps, in order
//!
//! 1. **Select `prev_events`**: the room's current forward extremities (`crate::actor::RoomActor`
//!    tracks these; for a locally originated event there is exactly one after every previous
//!    send, since this actor is the room's sole writer -- see the module docs on
//!    `crate::actor` for why that makes `depth` and `prev_events` trivial in the local case).
//! 2. **Select `auth_events`**: [`hs_state::auth::expected_auth_types`] names the `(type,
//!    state_key)` pairs that are *relevant*; [`select_auth_events`] resolves each one against the
//!    room's current state and includes only the ones that actually exist (the spec's auth events
//!    selection algorithm never invents a reference to a state key that has no value).
//! 3. **Fill in fixed fields**: `sender`, `room_id` (unless the room version's `m.room.create`
//!    omits it), `origin_server_ts`, `depth` (`1 + max(depth of prev_events)`), `state_key`,
//!    `content`, and -- for room versions 1 and 2 only -- an explicit `event_id` (room version 3
//!    onward derives it from the reference hash instead, which [`hs_model::event::Event::parse`]
//!    already does).
//! 4. **Size limit**: enforced by [`hs_model::event::Event::parse`]
//!    ([`hs_model::event::MAX_PDU_BYTES`], 64 KiB) once the event is assembled.
//! 5. **Hash and sign**: [`hs_model::hash::content_hash_base64`] into `hashes.sha256`, then
//!    [`hs_model::signing::sign_object`] under the homeserver's own signing key.
//! 4. **Authorize**: [`hs_state::auth::check_auth_events_selection`] (state-independent) then
//!    [`hs_state::auth::check_event_auth`] against the room's current state (state-dependent).
//!    This pipeline checks against exactly one snapshot -- the room's current state -- because a
//!    locally originated event's `prev_events` *is* the current forward extremities by
//!    construction (single-writer actor, no fork): the three-snapshot check the spec requires for
//!    an *inbound* event (implied-by-`auth_events`, before-the-event, current-at-receipt) collapses
//!    to one. Track 06 (federation) needs the general three-snapshot form for events it did not
//!    originate; that is the documented seam this module leaves (`docs/rfcs/0010-room-actor-state-store-seam.md`).
//!
//! Persisting the built [`Event`] (step 6 of the brief) is `crate::actor::RoomActor`'s job, not
//! this module's: this module only builds and authorizes.

use std::collections::{BTreeMap, HashMap};

use hs_model::Event;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use hs_model::hash;
use hs_model::ids::EventSn;
use hs_model::room_version::{EventsReferenceFormat, RoomVersionRules};
use hs_model::signing::{self, SigningKeyPair};
use hs_state::auth::{self, AuthEventRef, IncomingEvent};
use hs_state::state_fetch::{StateEntry, StateFetch};
use ruma::{EventId, OwnedEventId, RoomId, RoomVersionId, ServerName, UserId};

use crate::error::RoomError;

/// The room's current, flat, resolved state: `(event_type, state_key) -> EventSn`, plus the event
/// bodies needed to dereference an `EventSn` back into an event ID, sender and content.
///
/// This is `crate::actor::RoomActor`'s hot-state cache, borrowed for the duration of one pipeline
/// call. It directly implements [`StateFetch`] (`hs-state`'s auth-checking interface), which is
/// exactly the "production room actor implements `StateFetch` itself" seam `hs-state`'s own docs
/// anticipate (`crates/hs-state/src/state_fetch.rs`).
#[derive(Debug, Clone, Copy)]
pub struct CurrentState<'a> {
    /// `(event_type, state_key) -> EventSn`.
    pub state: &'a BTreeMap<(String, String), EventSn>,
    /// Every event body this room actor currently holds in memory, keyed by `EventSn`.
    pub events: &'a HashMap<EventSn, Event>,
}

impl<'a> CurrentState<'a> {
    /// The event that set `(event_type, state_key)` in the current state, if any.
    #[must_use]
    pub fn event_for(&self, event_type: &str, state_key: &str) -> Option<&'a Event> {
        let sn = self
            .state
            .get(&(event_type.to_owned(), state_key.to_owned()))?;
        self.events.get(sn)
    }
}

impl<'a> StateFetch for CurrentState<'a> {
    fn get(&self, event_type: &str, state_key: &str) -> Option<StateEntry<'a>> {
        let event = self.event_for(event_type, state_key)?;
        // `StateEntry::content` is the event's `content` sub-object, not the whole event JSON
        // (`event.json()`) -- every auth check reads fields like `membership` or `join_rule`
        // directly off `StateEntry::content`, so handing back the outer object silently makes
        // every one of those lookups fail as "missing field".
        let content = event.json().get("content")?.as_object()?;
        Some(StateEntry {
            sender: AsRef::<UserId>::as_ref(&event.header().sender),
            content,
        })
    }
}

/// A `prev_events`/`auth_events` reference to one already-persisted event: its ID and, for room
/// versions that embed one (`EventsReferenceFormat::V1WithHash`), its reference hash.
#[derive(Debug, Clone)]
pub struct EventRef {
    /// The referenced event's ID.
    pub event_id: OwnedEventId,
    /// The referenced event's reference hash, base64-encoded per the room version's alphabet.
    /// Only populated (and only used) for `EventsReferenceFormat::V1WithHash`.
    pub reference_hash_b64: Option<String>,
    /// The referenced event's `depth`, used to compute the new event's `depth`.
    pub depth: i64,
}

fn encode_ref(r: &EventRef, rules: &RoomVersionRules) -> serde_json::Value {
    match rules.events_reference_format {
        EventsReferenceFormat::V1WithHash => serde_json::json!([
            r.event_id.as_str(),
            { "sha256": r.reference_hash_b64.clone().unwrap_or_default() }
        ]),
        EventsReferenceFormat::V2IdOnly => serde_json::Value::String(r.event_id.to_string()),
    }
}

/// Builds an [`EventRef`] for an already-persisted event, computing its reference hash only when
/// the room version's reference format needs one (`EventsReferenceFormat::V1WithHash`).
///
/// # Errors
/// Returns [`RoomError::Redaction`] if computing the reference hash fails (cannot happen for an
/// event that already round-tripped through [`Event::parse`], but the possibility is preserved).
pub fn event_ref(event: &Event, rules: &RoomVersionRules) -> Result<EventRef, RoomError> {
    let reference_hash_b64 = match rules.events_reference_format {
        EventsReferenceFormat::V1WithHash => {
            Some(hash::encode_reference_hash(&event.reference_hash()?, rules))
        }
        EventsReferenceFormat::V2IdOnly => None,
    };
    Ok(EventRef {
        event_id: event.event_id().to_owned(),
        reference_hash_b64,
        depth: event.header().depth,
    })
}

/// Resolves [`hs_state::auth::expected_auth_types`] against the current state, including only the
/// pairs that currently have a value (the auth events selection algorithm never references a
/// state key with no event).
///
/// # Errors
/// Returns [`RoomError::Forbidden`] if `expected_auth_types` itself fails (a malformed
/// `m.room.member` event being authored, for example an unparsable `membership`).
pub fn select_auth_events(
    event: &IncomingEvent<'_>,
    rules: &RoomVersionRules,
    state: CurrentState<'_>,
) -> Result<Vec<EventRef>, RoomError> {
    let wanted = auth::expected_auth_types(event, rules).map_err(RoomError::from)?;
    let mut out = Vec::with_capacity(wanted.len());
    for (event_type, state_key) in wanted {
        if let Some(found) = state.event_for(&event_type, &state_key) {
            out.push(event_ref(found, rules)?);
        }
    }
    Ok(out)
}

/// Inputs for building a brand-new, locally originated event. `content` is caller-supplied,
/// already validated to be a JSON object (the HTTP layer does that before reaching this module).
#[derive(Debug, Clone)]
pub struct NewEvent {
    /// The event's `type`.
    pub event_type: String,
    /// The event's `state_key`, if this is a state event.
    pub state_key: Option<String>,
    /// The event's `sender`.
    pub sender: ruma::OwnedUserId,
    /// The event's `content`.
    pub content: serde_json::Value,
    /// For `m.room.redaction`: the event ID being redacted. Placed in `content.redacts` or the
    /// top-level `redacts` field per the room version's redaction rules.
    pub redacts: Option<OwnedEventId>,
}

/// Builds, hashes, signs and authorizes a new locally-originated event against the room's current
/// state. Does not persist it -- see `crate::actor::RoomActor`.
///
/// `prev_events` is the room's current forward extremities (already-persisted events); this
/// function computes `depth` as `1 + max(prev_events' depths)` (or `1` if there are none, i.e.
/// this is the room's `m.room.create`).
///
/// # Errors
/// Returns [`RoomError::InvalidEvent`] if the assembled event fails
/// [`hs_model::event::Event::parse`] (oversized, malformed), [`RoomError::Forbidden`] if
/// authorization rejects it, or [`RoomError::Signing`]/[`RoomError::Redaction`] for a hashing
/// failure.
#[allow(clippy::too_many_arguments)]
pub fn build_and_authorize(
    room_version: &RoomVersionId,
    rules: &RoomVersionRules,
    room_id: &RoomId,
    server_name: &ServerName,
    signing_key: &SigningKeyPair,
    now_ms: i64,
    prev_events: &[EventRef],
    state: CurrentState<'_>,
    new_event: NewEvent,
) -> Result<Event, RoomError> {
    let is_create = new_event.event_type == "m.room.create";

    let mut object = serde_json::Map::new();
    object.insert(
        "type".into(),
        serde_json::Value::String(new_event.event_type.clone()),
    );
    object.insert(
        "sender".into(),
        serde_json::Value::String(new_event.sender.to_string()),
    );
    object.insert("origin_server_ts".into(), serde_json::Value::from(now_ms));

    let depth = prev_events
        .iter()
        .map(|p| p.depth)
        .max()
        .map_or(1, |d| d + 1);
    object.insert("depth".into(), serde_json::Value::from(depth));

    if let Some(state_key) = &new_event.state_key {
        object.insert(
            "state_key".into(),
            serde_json::Value::String(state_key.clone()),
        );
    }

    let mut content = new_event.content.clone();
    if let Some(redacts) = &new_event.redacts
        && rules.redaction.content_field_redacts
        && let Some(map) = content.as_object_mut()
    {
        map.insert(
            "redacts".into(),
            serde_json::Value::String(redacts.to_string()),
        );
    }
    object.insert("content".into(), content);
    if let Some(redacts) = &new_event.redacts
        && !rules.redaction.content_field_redacts
    {
        object.insert(
            "redacts".into(),
            serde_json::Value::String(redacts.to_string()),
        );
    }

    let requires_room_id = !is_create || rules.event_format_requires_room_create_room_id;
    if requires_room_id {
        object.insert(
            "room_id".into(),
            serde_json::Value::String(room_id.to_string()),
        );
    }

    let prev_events_json: Vec<serde_json::Value> =
        prev_events.iter().map(|r| encode_ref(r, rules)).collect();
    object.insert(
        "prev_events".into(),
        serde_json::Value::Array(prev_events_json),
    );

    let incoming = IncomingEvent {
        event_type: &new_event.event_type,
        sender: AsRef::<UserId>::as_ref(&new_event.sender),
        room_id: Some(room_id),
        state_key: new_event.state_key.as_deref(),
        content: &to_canonical_object(
            &object.get("content").cloned().unwrap_or_default(),
            rules.strict_canonical_json,
        )
        .map_err(hs_model::EventError::from)?,
        prev_event_count: prev_events.len(),
        only_prev_event_is_room_create: prev_events.len() == 1
            && state.event_for("m.room.create", "").is_some_and(|c| {
                Some(c.event_id().to_owned()) == prev_events.first().map(|p| p.event_id.clone())
            }),
        event_id: None,
        redacts: new_event.redacts.as_deref(),
    };

    let auth_refs = if is_create {
        Vec::new()
    } else {
        select_auth_events(&incoming, rules, state)?
    };
    let auth_events_json: Vec<serde_json::Value> =
        auth_refs.iter().map(|r| encode_ref(r, rules)).collect();
    object.insert(
        "auth_events".into(),
        serde_json::Value::Array(auth_events_json),
    );

    if rules.event_format_requires_event_id {
        let generated = EventId::new_v1(server_name);
        object.insert(
            "event_id".into(),
            serde_json::Value::String(generated.to_string()),
        );
    }

    // --- hash and sign ---
    let mut canonical = to_canonical_object(
        &serde_json::Value::Object(object),
        rules.strict_canonical_json,
    )
    .map_err(hs_model::EventError::from)?;
    let content_hash = hash::content_hash_base64(&canonical);
    canonical.insert(
        "hashes".to_owned(),
        CanonicalJsonValue::Object(CanonicalJsonObject::from([(
            "sha256".to_owned(),
            CanonicalJsonValue::String(content_hash),
        )])),
    );
    signing::sign_object(&mut canonical, server_name, signing_key)?;

    let final_bytes = CanonicalJsonValue::Object(canonical).to_canonical_bytes();
    let final_value: serde_json::Value =
        serde_json::from_slice(&final_bytes).map_err(|e| RoomError::Internal(e.to_string()))?;

    let event = Event::parse(&final_value, room_version.clone())?;

    // --- authorize ---
    let auth_event_refs: Vec<AuthEventRef<'_>> = auth_refs
        .iter()
        .filter_map(|r| {
            state
                .events
                .values()
                .find(|e| e.event_id() == r.event_id)
                .map(|e| AuthEventRef {
                    event_type: &e.header().event_type,
                    state_key: e.header().state_key.as_deref().unwrap_or(""),
                    rejected: e.header().flags.is_rejected(),
                })
        })
        .collect();

    let create_lookup = || Ok(state.event_for("m.room.create", "").is_some());
    auth::check_auth_events_selection(rules, &incoming, &auth_event_refs, create_lookup)
        .map_err(RoomError::from)?;
    if !is_create {
        auth::check_event_auth(rules, &incoming, &state).map_err(RoomError::from)?;
    }

    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_model::room_version::{self};
    use ruma::{RoomId, ServerName, user_id};
    use std::collections::HashMap as StdHashMap;

    fn rules() -> RoomVersionRules {
        room_version::rules_for(&RoomVersionId::V11).unwrap()
    }

    fn key() -> SigningKeyPair {
        SigningKeyPair::generate("1")
    }

    #[test]
    fn builds_and_authorizes_a_create_event() {
        let server_owned = ServerName::parse("hs1").unwrap();
        let server: &ServerName = &server_owned;
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let creator = user_id!("@alice:hs1");
        let state = BTreeMap::new();
        let events = StdHashMap::new();

        let new_event = NewEvent {
            event_type: "m.room.create".to_owned(),
            state_key: Some(String::new()),
            sender: creator.to_owned(),
            content: serde_json::json!({"creator": creator, "room_version": "11"}),
            redacts: None,
        };

        let event = build_and_authorize(
            &RoomVersionId::V11,
            &rules(),
            &room_id,
            server,
            &key(),
            1,
            &[],
            CurrentState {
                state: &state,
                events: &events,
            },
            new_event,
        )
        .unwrap();

        assert_eq!(event.header().event_type, "m.room.create");
        assert!(event.event_id().as_str().starts_with('$'));
    }

    #[test]
    fn join_before_room_exists_is_rejected() {
        let server_owned = ServerName::parse("hs1").unwrap();
        let server: &ServerName = &server_owned;
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let alice = user_id!("@alice:hs1");
        let state = BTreeMap::new();
        let events = StdHashMap::new();

        let new_event = NewEvent {
            event_type: "m.room.member".to_owned(),
            state_key: Some(alice.to_string()),
            sender: alice.to_owned(),
            content: serde_json::json!({"membership": "join"}),
            redacts: None,
        };

        let err = build_and_authorize(
            &RoomVersionId::V11,
            &rules(),
            &room_id,
            server,
            &key(),
            2,
            &[],
            CurrentState {
                state: &state,
                events: &events,
            },
            new_event,
        )
        .unwrap_err();
        assert!(matches!(err, RoomError::Forbidden(_)));
    }
}
