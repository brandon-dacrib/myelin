//! State resolution v2 and v2.1 (room versions 2 to 12), delegated to `ruma-state-res`.
//!
//! `PLAN.md` D4: "State resolution uses `ruma-state-res` (v2 and v2.1) with an independent
//! implementation as a test oracle." This module is the thin adapter from this crate's
//! [`super::ResolutionEvent`]/[`super::EventStore`]/[`super::StateMap`] types to
//! `ruma_state_res::Event` and `ruma_state_res::resolve`.
//!
//! Because `ruma-state-res` resolves by Ruma's own [`ruma::RoomVersionId`] (it needs Ruma's
//! `AuthorizationRules`, which this crate's [`hs_model::room_version`] table does not try to
//! reproduce a conversion into), callers pass a [`ruma::RoomVersionId`] here rather than an
//! [`hs_model::room_version::RoomVersionRules`].
//!
//! `ruma_state_res::resolve` also needs each input state map's *auth chain* (every event
//! reachable by following `auth_events`) to compute the auth difference. This module computes
//! that by walking `auth_events` in [`super::EventStore`] directly; the chain-cover index
//! (`crate::chain_cover`) exists precisely to make that walk fast on a real room, but a resolver
//! given a small `EventStore` (as every caller of this module is, today) does not need it.

use std::collections::HashMap;

use ruma::events::{StateEventType, TimelineEventType};
use ruma::state_res::Event as RumaEvent;
use ruma::state_res::utils::event_id_set::EventIdSet;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId,
    RoomVersionId, UInt, UserId,
};
use serde_json::value::RawValue as RawJsonValue;

use super::{EventStore, ResolutionEvent, StateMap};
use crate::error::StateResError;

/// Resolves a set of state maps under state resolution v2 (room versions 2 to 11) or v2.1 (room
/// version 12 onward), via `ruma-state-res`.
///
/// # Errors
/// Returns [`StateResError::UnsupportedInput`] if `room_version` does not use state resolution
/// v2/v2.1, or if `ruma-state-res` itself reports an error (malformed events, a missing event).
pub fn resolve(
    room_version: &RoomVersionId,
    states: &[StateMap],
    store: &EventStore,
) -> Result<StateMap, StateResError> {
    let rules = room_version.rules().ok_or_else(|| {
        StateResError::UnsupportedInput(format!("unknown room version {room_version}"))
    })?;
    let state_res_rules = rules.state_res.v2_rules().cloned().ok_or_else(|| {
        StateResError::UnsupportedInput(format!(
            "room version {room_version} does not use state resolution v2"
        ))
    })?;

    let adapters: HashMap<OwnedEventId, Adapter> = store
        .iter()
        .map(|(id, event)| (id.clone(), Adapter::new(event)))
        .collect();

    let auth_chains: Vec<EventIdSet<OwnedEventId>> = states
        .iter()
        .map(|state| auth_chain_of(state.values(), store))
        .collect::<Result<_, _>>()?;

    let converted: Vec<ruma::state_res::StateMap<OwnedEventId>> = states
        .iter()
        .map(|state| {
            state
                .iter()
                .map(|((event_type, state_key), id)| {
                    (
                        (StateEventType::from(event_type.as_str()), state_key.clone()),
                        id.clone(),
                    )
                })
                .collect()
        })
        .collect();

    let result = ruma::state_res::resolve::<Adapter, _>(
        &rules.authorization,
        &state_res_rules,
        converted.iter(),
        auth_chains,
        |id: &EventId| adapters.get(id).cloned(),
        // Conflicted-state-subgraph inclusion (MSC4297, v2.1 only) is a Phase 1/2 concern per
        // `docs/workstreams/02-state-and-model.md` ("MSC4242 state DAGs are supported
        // experimentally from the first federation milestone", not Phase 0); until it lands, the
        // conflicted candidates themselves are returned unexpanded, which is conservative (it
        // under-includes rather than over-includes) and correct whenever the subgraph is empty,
        // which it is for every room that is not deliberately exercising MSC4297/MSC4242.
        |conflicted: &ruma::state_res::StateMap<Vec<OwnedEventId>>| {
            let mut set = EventIdSet::new();
            for ids in conflicted.values() {
                for id in ids {
                    set.insert(id.clone());
                }
            }
            Some(set)
        },
    )
    .map_err(|e| StateResError::UnsupportedInput(e.to_string()))?;

    Ok(result
        .into_iter()
        .map(|((event_type, state_key), id)| ((event_type.to_string(), state_key), id))
        .collect())
}

/// Walks `auth_events` transitively from every event in `ids`, within `store`.
fn auth_chain_of<'a>(
    ids: impl Iterator<Item = &'a OwnedEventId>,
    store: &EventStore,
) -> Result<EventIdSet<OwnedEventId>, StateResError> {
    let mut seen = EventIdSet::new();
    let mut stack: Vec<OwnedEventId> = ids.cloned().collect();
    while let Some(id) = stack.pop() {
        let Some(event) = store.get(&id) else {
            // An event outside the store (for example, a state event whose auth chain reaches
            // further back than what the caller loaded) is not itself an error: callers are
            // expected to load enough of the room's history for the room versions and scenarios
            // they resolve. We simply stop walking past it.
            continue;
        };
        for auth_id in &event.auth_events {
            if seen.insert(auth_id.clone()) {
                stack.push(auth_id.clone());
            }
        }
    }
    Ok(seen)
}

/// Adapts [`ResolutionEvent`] to `ruma_state_res::Event`.
#[derive(Clone)]
struct Adapter {
    id: OwnedEventId,
    room_id: OwnedRoomId,
    sender: OwnedUserId,
    event_type: TimelineEventType,
    content: std::sync::Arc<RawJsonValue>,
    state_key: String,
    origin_server_ts: i64,
    prev_events: Vec<OwnedEventId>,
    auth_events: Vec<OwnedEventId>,
}

impl Adapter {
    fn new(event: &ResolutionEvent) -> Self {
        let content_value = hs_model::canonical::CanonicalJsonValue::Object(event.content.clone());
        // `to_canonical_bytes()` always emits well-formed JSON syntax (see `hs_model::canonical`),
        // so parsing it back can never fail; and a `serde_json::Value` freshly parsed from JSON
        // text always re-serializes via `to_raw_value`. Both `expect`s are unreachable in
        // practice, not user-triggerable, and exist only to avoid propagating an error type this
        // adapter constructor has no good way to return (`ruma_state_res::Event::content()` is
        // infallible).
        let raw = serde_json::value::to_raw_value(
            &serde_json::from_slice::<serde_json::Value>(&content_value.to_canonical_bytes())
                .expect("canonical JSON is valid JSON"),
        )
        .expect("canonical JSON re-serializes");
        Self {
            id: event.event_id.clone(),
            room_id: event.room_id.clone(),
            sender: event.sender.clone(),
            event_type: TimelineEventType::from(event.event_type.as_str()),
            content: std::sync::Arc::from(raw),
            state_key: event.state_key.clone(),
            origin_server_ts: event.origin_server_ts,
            prev_events: event.prev_events.clone(),
            auth_events: event.auth_events.clone(),
        }
    }
}

impl RumaEvent for Adapter {
    type Id = OwnedEventId;

    fn event_id(&self) -> &Self::Id {
        &self.id
    }

    fn room_id(&self) -> Option<&RoomId> {
        Some(&self.room_id)
    }

    fn sender(&self) -> &UserId {
        &self.sender
    }

    fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
        let ts = u32::try_from(self.origin_server_ts.max(0)).unwrap_or(u32::MAX);
        MilliSecondsSinceUnixEpoch(UInt::from(ts))
    }

    fn event_type(&self) -> &TimelineEventType {
        &self.event_type
    }

    fn content(&self) -> &RawJsonValue {
        &self.content
    }

    fn state_key(&self) -> Option<&str> {
        Some(&self.state_key)
    }

    fn prev_events(&self) -> Box<dyn DoubleEndedIterator<Item = &Self::Id> + '_> {
        Box::new(self.prev_events.iter())
    }

    fn auth_events(&self) -> Box<dyn DoubleEndedIterator<Item = &Self::Id> + '_> {
        Box::new(self.auth_events.iter())
    }

    fn redacts(&self) -> Option<&Self::Id> {
        None
    }

    fn rejected(&self) -> bool {
        false
    }
}
