//! The event creation pipeline: PLAN.md section 5.3's "event creation, authorization against
//! current state" -- everything between "a client asked to send this content" and "here is a
//! signed, authorized [`Event`] ready to persist".
//!
//! # Steps, in order
//!
//! 1. **Select `prev_events`**: the room's current forward extremities (`crate::actor::RoomActor`
//!    tracks these; for a locally originated event there is exactly one after every previous
//!    send, since ordinary local sends always cite -- and thereby converge -- every current
//!    extremity. A genuine fork (more than one forward extremity at once) is possible when an
//!    event is persisted that does not cite every extremity -- see
//!    `crate::actor::RoomActor::send_event_citing` -- and is resolved through
//!    [`hs_state::api::StateStore::resolve`] via [`RoomStateView`], not assumed away.
//! 2. **Select `auth_events`**: [`hs_state::auth::expected_auth_types`] names the `(type,
//!    state_key)` pairs that are *relevant*; [`select_auth_events`] resolves each one against the
//!    room's current state and includes only the ones that actually exist (the spec's auth events
//!    selection algorithm never invents a reference to a state key that has no value).
//! 3. **Fill in fixed fields**: `sender`, `room_id` (unless the room version's `m.room.create`
//!    omits it, per MSC4291/room version 12), `origin_server_ts`, `depth` (`1 + max(depth of
//!    prev_events)`), `state_key`, `content`, and -- for room versions 1 and 2 only -- an explicit
//!    `event_id` (room version 3 onward derives it from the reference hash instead, which
//!    [`hs_model::event::Event::parse`] already does).
//! 4. **Size limit**: enforced by [`hs_model::event::Event::parse`]
//!    ([`hs_model::event::MAX_PDU_BYTES`], 64 KiB) once the event is assembled.
//! 5. **Hash and sign**: [`hs_model::hash::content_hash_base64`] into `hashes.sha256`, then
//!    [`hs_model::signing::sign_object`] under the homeserver's own signing key.
//! 4. **Authorize**: [`hs_state::auth::check_auth_events_selection`] (state-independent) then
//!    [`hs_state::auth::check_event_auth`] against the room's current state (state-dependent),
//!    read through [`RoomStateView`] -- a thin view over `hs_state`'s production
//!    [`hs_state::api::StateStore`], not a materialized map. For a locally originated event this
//!    checks against exactly one resolved snapshot: the resolution of every event's cited
//!    `prev_events` (a no-op resolve when there is only one, per
//!    [`hs_state::api::StateStore::current_state`]'s documented behavior). The general
//!    three-snapshot check the spec requires for an *inbound* federation event
//!    (implied-by-`auth_events`, before-the-event, current-at-receipt) is still track 06's job --
//!    see `docs/design/04-room-actor-protocol.md`'s `Command::PersistInbound`.
//!
//! Persisting the built [`Event`] (step 6 of the brief) is `crate::actor::RoomActor`'s job, not
//! this module's: this module only builds and authorizes.

use std::collections::HashMap;

use hs_model::Event;
use hs_model::canonical::{CanonicalJsonObject, CanonicalJsonValue, to_canonical_object};
use hs_model::hash;
use hs_model::ids::EventSn;
use hs_model::redaction;
use hs_model::room_version::{EventsReferenceFormat, RoomVersionRules};
use hs_model::signing::{self, SigningKeyPair};
use hs_state::api::StateStore;
use hs_state::auth::{self, AuthEventRef, IncomingEvent};
use hs_state::error::AuthError;
use hs_state::state_fetch::{EventBody, StoreStateFetch};
use ruma::{EventId, OwnedEventId, RoomId, RoomVersionId, ServerName, UserId};

use crate::error::RoomError;

/// Adapts the room actor's in-memory event-body cache (`HashMap<EventSn, Event>`) into
/// [`hs_state::state_fetch::EventBody`], the narrow "dereference an `EventSn` back into a sender
/// and content" interface [`StoreStateFetch`] needs. Per
/// `docs/status/02-state-and-model.md`'s "exactly what track 04 calls to delete `CurrentState`":
/// the room actor reads through its own cache directly rather than copying bodies into
/// `hs_state::state_fetch::EventBodies` (a test fixture type).
#[derive(Debug, Clone, Copy)]
pub struct EventMap<'a>(pub &'a HashMap<EventSn, Event>);

impl<'a> EventBody for EventMap<'a> {
    fn body(&self, event: EventSn) -> Option<(&UserId, &CanonicalJsonObject)> {
        let e = self.0.get(&event)?;
        let content = e.json().get("content")?.as_object()?;
        Some((AsRef::<UserId>::as_ref(&e.header().sender), content))
    }
}

/// The room's current, resolved state as seen through `hs_state`'s production
/// [`StateStore`]: a `(store, root)` pair plus the event-body cache needed to dereference a
/// [`StateStore::get`] result back into a full [`Event`] (for `event_id`/`depth`/reference-hash,
/// which [`StateFetch`] itself does not carry -- it only hands back `sender`/`content`).
///
/// Replaces `CurrentState`, the flat `(event_type, state_key) -> EventSn` map this crate's first
/// pass used (`docs/rfcs/0010-room-actor-state-store-seam.md`): a `RoomStateView` never
/// materializes anything beyond the single entry a lookup asks for, and -- unlike the flat map --
/// its `root` can be the *resolution* of several forward extremities, which is what makes a
/// genuine fork representable at all.
pub struct RoomStateView<'a, S: StateStore> {
    /// The state store this view reads through.
    pub store: &'a S,
    /// The resolved state to read: either one event's `state_at`, or the `resolve()` of several.
    pub root: S::Root,
    /// This room actor's in-memory event-body cache.
    pub bodies: EventMap<'a>,
}

impl<'a, S: StateStore> RoomStateView<'a, S> {
    /// The event that set `(event_type, state_key)` in this view's state, if any.
    ///
    /// # Errors
    /// Returns `S::Error` on a storage-layer failure from `store.intern_state_key`/`store.get`.
    pub fn event_for(
        &self,
        event_type: &str,
        state_key: &str,
    ) -> Result<Option<&'a Event>, S::Error> {
        let key = self.store.intern_state_key(event_type, state_key)?;
        let sn = self.store.get(self.root, key)?;
        Ok(sn.and_then(|sn| self.bodies.0.get(&sn)))
    }

    /// Adapts this view into [`StateFetch`], the narrow interface event authorization reads
    /// through ([`hs_state::auth::check_event_auth`]).
    #[must_use]
    pub fn state_fetch(&self) -> StoreStateFetch<'_, S, EventMap<'a>> {
        StoreStateFetch::new(self.store, self.root, &self.bodies)
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

/// Decodes an `auth_events`/`prev_events` JSON array back into plain event IDs, accepting either
/// wire shape: `["$id", ...]` (`EventsReferenceFormat::V2IdOnly`, room version 3 onward) or
/// `[["$id", {"sha256": "..."}], ...]` (`EventsReferenceFormat::V1WithHash`, room versions 1-2).
///
/// This is how `crate::actor::RoomActor` recovers an already-built event's `EventSn` ancestors
/// (for `hs_state::api::StateStore::add_event` and forward-extremity bookkeeping) from the event's
/// own serialized fields, rather than needing a second, parallel representation of "what this
/// event cites" carried alongside it. Entries this actor does not recognize (should not happen for
/// a locally originated or previously accepted event) are silently skipped, not an error: a
/// best-effort decode is exactly as much as forward-extremity/state-store bookkeeping needs, and a
/// missing ancestor is already a broken invariant the caller's own `EventSn` lookup will notice.
#[must_use]
pub fn decode_event_ids(value: Option<&CanonicalJsonValue>) -> Vec<OwnedEventId> {
    let Some(items) = value.and_then(CanonicalJsonValue::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| item.as_str().or_else(|| item.as_array()?.first()?.as_str()))
        .filter_map(|s| EventId::parse(s).ok())
        .map(|id| id.to_owned())
        .collect()
}

/// Resolves [`hs_state::auth::expected_auth_types`] against the current state, including only the
/// pairs that currently have a value (the auth events selection algorithm never references a
/// state key with no event).
///
/// # Errors
/// Returns [`RoomError::Forbidden`] if `expected_auth_types` itself fails (a malformed
/// `m.room.member` event being authored, for example an unparsable `membership`), or
/// [`RoomError::State`] if the state store fails.
pub fn select_auth_events<S: StateStore>(
    event: &IncomingEvent<'_>,
    rules: &RoomVersionRules,
    state: &RoomStateView<'_, S>,
) -> Result<Vec<EventRef>, RoomError> {
    let wanted = auth::expected_auth_types(event, rules).map_err(RoomError::from)?;
    let mut out = Vec::with_capacity(wanted.len());
    for (event_type, state_key) in wanted {
        let found = state
            .event_for(&event_type, &state_key)
            .map_err(|e| RoomError::State(e.to_string()))?;
        if let Some(found) = found {
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
/// `prev_events` is the set of events this new event cites as its ancestors (ordinarily the
/// room's current forward extremities -- see `crate::actor::RoomActor::send_event` -- but see
/// `crate::actor::RoomActor::send_event_citing` for why this can be a strict subset); this
/// function computes `depth` as `1 + max(prev_events' depths)` (or `1` if there are none, i.e.
/// this is the room's `m.room.create`). `room_id` is `None` only for a room version 12+
/// `m.room.create` event (MSC4291: the room ID is derived from this event's own reference hash
/// *after* it is built, so it cannot be known yet when building it) -- every other event, in every
/// room version, must supply one.
///
/// # Errors
/// Returns [`RoomError::InvalidEvent`] if the assembled event fails
/// [`hs_model::event::Event::parse`] (oversized, malformed), [`RoomError::Forbidden`] if
/// authorization rejects it, [`RoomError::State`] if the state store fails, or
/// [`RoomError::Signing`]/[`RoomError::Redaction`] for a hashing failure.
#[allow(clippy::too_many_arguments)]
pub fn build_and_authorize<S: StateStore>(
    room_version: &RoomVersionId,
    rules: &RoomVersionRules,
    room_id: Option<&RoomId>,
    server_name: &ServerName,
    signing_key: &SigningKeyPair,
    now_ms: i64,
    prev_events: &[EventRef],
    state: &RoomStateView<'_, S>,
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
        let room_id = room_id.ok_or_else(|| {
            RoomError::Internal("build_and_authorize: room_id required but not supplied".into())
        })?;
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

    let create_event_for_view = state
        .event_for("m.room.create", "")
        .map_err(|e| RoomError::State(e.to_string()))?;

    let incoming = IncomingEvent {
        event_type: &new_event.event_type,
        sender: AsRef::<UserId>::as_ref(&new_event.sender),
        room_id,
        state_key: new_event.state_key.as_deref(),
        content: &to_canonical_object(
            &object.get("content").cloned().unwrap_or_default(),
            rules.strict_canonical_json,
        )
        .map_err(hs_model::EventError::from)?,
        prev_event_count: prev_events.len(),
        only_prev_event_is_room_create: prev_events.len() == 1
            && create_event_for_view.is_some_and(|c| {
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
    // Sign the *redacted* form, not the full event -- the spec's algorithm
    // (`refs/matrix-spec/content/server-server-api.md`, "Adding hashes and signatures to outgoing
    // events"): hash the full event (above), redact, sign the redacted object, then copy the
    // resulting signature back onto the original, unredacted object this function returns and
    // persists. A spec-compliant verifier always redacts *before* checking a signature (the
    // matching "Validating hashes and signatures on received events" text), so signing the full
    // object instead -- what this line used to do -- produces a signature that verifies only
    // against this server's own unredacted copy, and mismatches for any event type whose content
    // redaction does not fully retain (an ordinary `m.room.message`'s `content` most of all: see
    // `hs_model::redaction::redact_content`). See `docs/rfcs/0014-event-signing-must-sign-the-redacted-form.md`
    // for the full writeup (discovered by track 06 fixing the symmetric bug on the verification
    // side, `hs_federation::inbound::verify_pdu`).
    let mut redacted =
        redaction::redact(&canonical, &rules.redaction).map_err(hs_model::EventError::from)?;
    signing::sign_object(&mut redacted, server_name, signing_key)?;
    canonical.insert(
        "signatures".to_owned(),
        redacted
            .remove("signatures")
            .expect("sign_object always inserts a signature"),
    );

    let final_bytes = CanonicalJsonValue::Object(canonical).to_canonical_bytes();
    let final_value: serde_json::Value =
        serde_json::from_slice(&final_bytes).map_err(|e| RoomError::Internal(e.to_string()))?;

    let event = Event::parse(&final_value, room_version.clone())?;

    // --- authorize ---
    let auth_event_refs: Vec<AuthEventRef<'_>> = auth_refs
        .iter()
        .filter_map(|r| {
            state
                .bodies
                .0
                .values()
                .find(|e| e.event_id() == r.event_id)
                .map(|e| AuthEventRef {
                    event_type: &e.header().event_type,
                    state_key: e.header().state_key.as_deref().unwrap_or(""),
                    rejected: e.header().flags.is_rejected(),
                })
        })
        .collect();

    let create_lookup = || {
        state
            .event_for("m.room.create", "")
            .map(|found| found.is_some())
            .map_err(|e| AuthError::reject(e.to_string()))
    };
    auth::check_auth_events_selection(rules, &incoming, &auth_event_refs, create_lookup)
        .map_err(RoomError::from)?;
    if !is_create {
        auth::check_event_auth(rules, &incoming, &state.state_fetch()).map_err(RoomError::from)?;
    }

    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_model::room_version::{self};
    use hs_state::store::InMemoryStateStore;
    use ruma::{RoomId, ServerName, user_id};
    use std::collections::HashMap as StdHashMap;

    fn rules() -> RoomVersionRules {
        room_version::rules_for(&RoomVersionId::V11).unwrap()
    }

    fn key() -> SigningKeyPair {
        SigningKeyPair::generate("1")
    }

    /// An empty view: no state, no event bodies -- what a brand-new room's `m.room.create` (or
    /// any event authorized against a room that does not yet exist) is built against.
    fn empty_view<'a>(
        store: &'a InMemoryStateStore,
        events: &'a StdHashMap<EventSn, Event>,
    ) -> RoomStateView<'a, InMemoryStateStore> {
        RoomStateView {
            store,
            root: store.empty_root(),
            bodies: EventMap(events),
        }
    }

    #[test]
    fn builds_and_authorizes_a_create_event() {
        let server_owned = ServerName::parse("hs1").unwrap();
        let server: &ServerName = &server_owned;
        let room_id = RoomId::parse("!r:hs1").unwrap();
        let creator = user_id!("@alice:hs1");
        let store = InMemoryStateStore::new(RoomVersionId::V11).unwrap();
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
            Some(&room_id),
            server,
            &key(),
            1,
            &[],
            &empty_view(&store, &events),
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
        let store = InMemoryStateStore::new(RoomVersionId::V11).unwrap();
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
            Some(&room_id),
            server,
            &key(),
            2,
            &[],
            &empty_view(&store, &events),
            new_event,
        )
        .unwrap_err();
        assert!(matches!(err, RoomError::Forbidden(_)));
    }

    #[test]
    fn decode_event_ids_handles_both_reference_formats() {
        let v1_style =
            serde_json::json!([["$a:hs1", {"sha256": "x"}], ["$b:hs1", {"sha256": "y"}]]);
        let v1_canonical = to_canonical_object(&serde_json::json!({"x": v1_style}), true).unwrap();
        let decoded = decode_event_ids(v1_canonical.get("x"));
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].as_str(), "$a:hs1");

        let v2_style = serde_json::json!(["$a:hs1", "$b:hs1"]);
        let v2_canonical = to_canonical_object(&serde_json::json!({"x": v2_style}), true).unwrap();
        let decoded = decode_event_ids(v2_canonical.get("x"));
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[1].as_str(), "$b:hs1");
    }
}
