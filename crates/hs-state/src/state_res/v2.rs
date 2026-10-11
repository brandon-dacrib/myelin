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
//! reachable by following `auth_events`) to compute the auth difference. This module asks the
//! [`super::EventFetch`] for it ([`super::EventFetch::auth_chain`]): the in-memory
//! [`super::EventStore`] walks `auth_events` directly, and the production store answers from
//! its chain-cover index (`crate::chain_cover`), which exists precisely to make that walk fast
//! on a real room. Events are fetched on demand, and only those the resolution touches.

use std::cell::RefCell;
use std::collections::HashMap;

use ruma::events::{StateEventType, TimelineEventType};
use ruma::state_res::Event as RumaEvent;
use ruma::state_res::utils::event_id_set::EventIdSet;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId,
    RoomVersionId, UInt, UserId,
};
use serde_json::value::RawValue as RawJsonValue;

use super::{EventFetch, ResolutionEvent, StateMap};
use crate::error::StateResError;

/// Resolves a set of state maps under state resolution v2 (room versions 2 to 11) or v2.1 (room
/// version 12 onward), via `ruma-state-res`.
///
/// # Errors
/// Returns [`StateResError::UnsupportedInput`] if `room_version` does not use state resolution
/// v2/v2.1, or if `ruma-state-res` itself reports an error (malformed events, a missing event).
pub fn resolve<F: EventFetch + ?Sized>(
    room_version: &RoomVersionId,
    states: &[StateMap],
    store: &F,
) -> Result<StateMap, StateResError> {
    let rules = room_version.rules().ok_or_else(|| {
        StateResError::UnsupportedInput(format!("unknown room version {room_version}"))
    })?;
    let state_res_rules = rules.state_res.v2_rules().cloned().ok_or_else(|| {
        StateResError::UnsupportedInput(format!(
            "room version {room_version} does not use state resolution v2"
        ))
    })?;

    // Adapted on first fetch and memoised: `ruma-state-res` asks for the same event many times
    // (every auth check of every conflicted event re-reads its auth events), and adapting one
    // re-serialises its content.
    let adapters: RefCell<HashMap<OwnedEventId, Adapter>> = RefCell::new(HashMap::new());
    let fetch_event = |id: &EventId| -> Option<Adapter> {
        if let Some(adapter) = adapters.borrow().get(id) {
            return Some(adapter.clone());
        }
        let adapter = Adapter::new(store.fetch(id)?);
        adapters.borrow_mut().insert(id.to_owned(), adapter.clone());
        Some(adapter)
    };

    let auth_chains: Vec<EventIdSet<OwnedEventId>> = states
        .iter()
        .map(|state| {
            let ids: Vec<OwnedEventId> = state.values().cloned().collect();
            store.auth_chain(&ids)
        })
        .collect();

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
        fetch_event,
        // The conflicted state subgraph (MSC4297, state resolution v2.1): the conflicted events
        // and every event on an `auth_events` path from one to another. `ruma-state-res` asks
        // for it only under v2.1. Until 2026-10-04 the conflicted events alone were answered,
        // which loses an event between two conflicted ones that the full conflicted set needs
        // (Complement's `TestMSC4297StateResolutionV2_1_includes_conflicted_subgraph`).
        |conflicted: &ruma::state_res::StateMap<Vec<OwnedEventId>>| {
            let ids: Vec<OwnedEventId> = conflicted.values().flatten().cloned().collect();
            Some(conflicted_subgraph(&ids, store))
        },
    )
    .map_err(|e| StateResError::UnsupportedInput(e.to_string()))?;

    Ok(result
        .into_iter()
        .map(|((event_type, state_key), id)| ((event_type.to_string(), state_key), id))
        .collect())
}

/// The conflicted state subgraph of `conflicted` (MSC4297): those events, and every event that
/// is an `auth_events` ancestor of one of them and has another of them among its own
/// ancestors -- that is, lies on a path between two. Walked within `store`, each event once.
fn conflicted_subgraph<F: EventFetch + ?Sized>(
    conflicted: &[OwnedEventId],
    store: &F,
) -> EventIdSet<OwnedEventId> {
    let targets: std::collections::HashSet<&OwnedEventId> = conflicted.iter().collect();
    // Every strict ancestor of a conflicted event.
    let ancestors = store.auth_chain(conflicted);
    // Whether an event is conflicted or reaches one through `auth_events`, memoised; the
    // walk is iterative so a long auth chain does not exhaust the stack.
    let mut reaches: HashMap<OwnedEventId, bool> = HashMap::new();
    let mut set = EventIdSet::new();
    for id in conflicted {
        set.insert(id.clone());
    }
    for start in ancestors.iter() {
        let mut stack: Vec<(OwnedEventId, bool)> = vec![(start.clone(), false)];
        while let Some((id, expanded)) = stack.pop() {
            if reaches.contains_key(&id) {
                continue;
            }
            if targets.contains(&id) {
                reaches.insert(id, true);
                continue;
            }
            let auth: Vec<OwnedEventId> = store
                .fetch(&id)
                .map(|event| event.auth_events.clone())
                .unwrap_or_default();
            if expanded {
                let any = auth
                    .iter()
                    .any(|a| reaches.get(a).copied().unwrap_or(false));
                reaches.insert(id, any);
            } else {
                stack.push((id, true));
                for a in auth {
                    if !reaches.contains_key(&a) {
                        stack.push((a, false));
                    }
                }
            }
        }
        if reaches.get(start).copied().unwrap_or(false) {
            set.insert(start.clone());
        }
    }
    set
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
        // The whole millisecond timestamp, saturated at JavaScript's safe-integer maximum
        // (`UInt::MAX`, 2^53 - 1), which every real timestamp is far below. This used to go
        // through `u32`, which every timestamp since 2004 overflows: each real event then
        // carried `u32::MAX`, every mainline tie broke on event ID instead of time, and a leave
        // forked from a power-levels change lost to the join it superseded on about half of
        // all rooms (Complement's `TestRestrictedRoomsRemoteJoinFailOver`, flapping).
        let ts = u64::try_from(self.origin_server_ts).unwrap_or(0);
        MilliSecondsSinceUnixEpoch(UInt::new_saturating(ts))
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

#[cfg(test)]
mod tests {
    use hs_model::room_version;
    use ruma::RoomVersionId;
    use serde_json::json;

    use super::super::test_support::RoomBuilder;

    /// MSC4297: with conflicted power levels A and C, where C's auth chain runs through B to A,
    /// B lies between them and is in the conflicted subgraph; the create event, below A, is not.
    #[test]
    fn an_event_between_two_conflicted_events_is_in_the_subgraph() {
        let rules = room_version::rules_for(&RoomVersionId::V12).unwrap();
        let mut builder = RoomBuilder::new(rules);
        let (mut state, tip, [creator, _, _]) = builder.genesis();
        let create = state
            .get(&("m.room.create".to_owned(), String::new()))
            .unwrap()
            .clone();
        let a = builder.push(
            &mut state,
            Some(tip),
            10,
            "m.room.power_levels",
            "",
            &creator,
            json!({"users": {}, "state_default": 51}),
        );
        let b = builder.push(
            &mut state,
            Some(a.clone()),
            11,
            "m.room.power_levels",
            "",
            &creator,
            json!({"users": {}, "state_default": 52}),
        );
        let c = builder.push(
            &mut state,
            Some(b.clone()),
            12,
            "m.room.power_levels",
            "",
            &creator,
            json!({"users": {}, "state_default": 53}),
        );
        let subgraph = super::conflicted_subgraph(&[a.clone(), c.clone()], &builder.store);
        assert!(subgraph.contains(&a) && subgraph.contains(&c));
        assert!(subgraph.contains(&b), "the event between them is left out");
        assert!(
            !subgraph.contains(&create),
            "the create event is not between them"
        );
    }
}
