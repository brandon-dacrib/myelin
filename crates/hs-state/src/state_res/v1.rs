//! State resolution v1 (room version 1), in-house.
//!
//! Re-expressed directly from the "State resolution" section of the room version 1 specification
//! page (`refs/matrix-spec/content/rooms/v1.md#state-resolution`, Apache-2.0):
//!
//! > The resolved state is built up in a number of passes; here we use *R* to refer to the
//! > results of the resolution so far.
//! >
//! > - Start by setting *R* to the union of the states to be resolved, excluding any
//! >   *conflicting* events.
//! > - First we resolve conflicts between `m.room.power_levels` events. If there is no conflict,
//! >   this step is skipped, otherwise:
//! >     - Assemble all the `m.room.power_levels` events from the states to be resolved into a
//! >       list.
//! >     - Sort the list by ascending `depth` then descending `sha1(event_id)`.
//! >     - Add the first event in the list to *R*.
//! >     - For each subsequent event in the list, check that the event would be allowed by the
//! >       authorisation rules for a room in state *R*. If the event would be allowed, then
//! >       update *R* with the event and continue with the next event in the list. If it would
//! >       not be allowed, stop and continue below with `m.room.join_rules` events.
//! > - Repeat the above process for conflicts between `m.room.join_rules` events.
//! > - Repeat the above process for conflicts between `m.room.member` events.
//! > - No other events affect the authorisation rules, so for all other conflicts, just pick the
//! >   event with the highest depth and lowest `sha1(event_id)` that passes authentication in *R*
//! >   and add it to *R*.
//!
//! This is known to be buggy by design (room version 2 replaced it precisely to fix the resets
//! the spec page itself warns about); it is implemented here only because federation with
//! existing room-version-1 rooms requires it, matching `PLAN.md` D4.

use hs_model::room_version::RoomVersionRules;
use ruma::OwnedEventId;

use super::{
    EventStore, MapStateFetch, StateMap, incoming_event, sha1_of_event_id, split_conflicted,
};
use crate::auth;
use crate::error::StateResError;

/// Resolves a set of state maps under state resolution v1.
///
/// # Errors
/// Returns [`StateResError::MissingEvent`] if a candidate event referenced by one of `states` is
/// not present in `store`.
pub fn resolve(
    rules: &RoomVersionRules,
    states: &[StateMap],
    store: &EventStore,
) -> Result<StateMap, StateResError> {
    let (mut r, mut conflicted) = split_conflicted(states);

    // The power-affecting types are resolved first, each as one combined, depth-sorted list; a
    // failed auth check stops that type's list (leaving any remaining conflicted keys of that
    // type to the generic "all other conflicts" pass below).
    for event_type in ["m.room.power_levels", "m.room.join_rules", "m.room.member"] {
        resolve_power_affecting_type(rules, event_type, &mut r, &mut conflicted, store)?;
    }

    // All other conflicts: independently, per key, the highest-depth/lowest-sha1 candidate that
    // passes authorization against R.
    for (key, ids) in conflicted {
        if let Some(winner) = pick_best_passing(rules, &ids, &r, store)? {
            r.insert(key, winner);
        }
        // If nothing passes, the key is left out of R. The spec does not define this case (it
        // should not arise for well-formed rooms: these are non-power-affecting types, whose only
        // state-dependent rule is the generic power-level gate, and R already contains a
        // consistent m.room.power_levels by this point); leaving it unresolved rather than
        // guessing is the safer default.
    }

    Ok(r)
}

fn resolve_power_affecting_type(
    rules: &RoomVersionRules,
    event_type: &str,
    r: &mut StateMap,
    conflicted: &mut std::collections::BTreeMap<(String, String), Vec<OwnedEventId>>,
    store: &EventStore,
) -> Result<(), StateResError> {
    let mut combined: Vec<OwnedEventId> = Vec::new();
    let mut keys_of_type: Vec<(String, String)> = Vec::new();
    for (key, ids) in conflicted.iter() {
        if key.0 == event_type {
            combined.extend(ids.iter().cloned());
            keys_of_type.push(key.clone());
        }
    }
    if combined.is_empty() {
        return Ok(());
    }

    let ordered = sort_ascending_depth_descending_sha1(&combined, store)?;
    let mut iter = ordered.into_iter();

    let Some(first) = iter.next() else {
        return Ok(());
    };
    let first_event = get(store, &first)?;
    let first_key = (
        first_event.event_type.clone(),
        first_event.state_key.clone(),
    );
    r.insert(first_key.clone(), first);
    conflicted.remove(&first_key);

    for candidate in iter {
        let event = get(store, &candidate)?;
        let key = (event.event_type.clone(), event.state_key.clone());
        let fetch = MapStateFetch { map: r, store };
        let incoming = incoming_event(event);
        if auth::check_event_auth(rules, &incoming, &fetch).is_ok() {
            r.insert(key.clone(), candidate);
            conflicted.remove(&key);
        } else {
            // Stop processing this type's list; any keys of this type not yet added stay
            // conflicted and fall through to the generic pass.
            break;
        }
    }

    let _ = keys_of_type;
    Ok(())
}

/// Picks the highest-depth, lowest-`sha1(event_id)` candidate that passes authorization against
/// the given state, or `None` if no candidate passes.
fn pick_best_passing(
    rules: &RoomVersionRules,
    ids: &[OwnedEventId],
    r: &StateMap,
    store: &EventStore,
) -> Result<Option<OwnedEventId>, StateResError> {
    let mut passing = Vec::new();
    for id in ids {
        let event = get(store, id)?;
        let fetch = MapStateFetch { map: r, store };
        let incoming = incoming_event(event);
        if auth::check_event_auth(rules, &incoming, &fetch).is_ok() {
            passing.push(id.clone());
        }
    }
    if passing.is_empty() {
        return Ok(None);
    }
    let ordered = sort_descending_depth_ascending_sha1(&passing, store)?;
    Ok(ordered.into_iter().next())
}

fn get<'a>(
    store: &'a EventStore,
    id: &OwnedEventId,
) -> Result<&'a super::ResolutionEvent, StateResError> {
    store
        .get(id)
        .ok_or_else(|| StateResError::MissingEvent(id.to_string()))
}

/// Ascending `depth`, ties broken by descending `sha1(event_id)`.
fn sort_ascending_depth_descending_sha1(
    ids: &[OwnedEventId],
    store: &EventStore,
) -> Result<Vec<OwnedEventId>, StateResError> {
    let mut v = ids.to_vec();
    let mut err = None;
    v.sort_by(|a, b| {
        let (da, db) = match (get(store, a), get(store, b)) {
            (Ok(ea), Ok(eb)) => (ea.depth, eb.depth),
            _ => {
                err = Some(());
                (0, 0)
            }
        };
        da.cmp(&db)
            .then_with(|| sha1_of_event_id(b).cmp(&sha1_of_event_id(a)))
    });
    if err.is_some() {
        // Re-run to surface the exact missing ID as an error.
        for id in ids {
            get(store, id)?;
        }
    }
    Ok(v)
}

/// Descending `depth`, ties broken by ascending `sha1(event_id)`.
fn sort_descending_depth_ascending_sha1(
    ids: &[OwnedEventId],
    store: &EventStore,
) -> Result<Vec<OwnedEventId>, StateResError> {
    let mut v = ids.to_vec();
    v.sort_by(|a, b| {
        let ea = &store[a];
        let eb = &store[b];
        eb.depth
            .cmp(&ea.depth)
            .then_with(|| sha1_of_event_id(a).cmp(&sha1_of_event_id(b)))
    });
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use crate::state_res::ResolutionEvent;
    use hs_model::canonical::to_canonical_object;
    use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
    use serde_json::json;

    struct Builder {
        room_id: OwnedRoomId,
        rules: RoomVersionRules,
        store: EventStore,
        n: u32,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                room_id: RoomId::parse("!r:hs1").unwrap(),
                rules: RoomVersionRules::V6,
                store: EventStore::new(),
                n: 0,
            }
        }

        fn id(&mut self) -> OwnedEventId {
            self.n += 1;
            OwnedEventId::try_from(format!("$v1e{}:hs1", self.n).as_str()).unwrap()
        }

        #[allow(clippy::too_many_arguments)]
        fn push(
            &mut self,
            state: &mut StateMap,
            prev: Option<OwnedEventId>,
            depth: i64,
            event_type: &str,
            state_key: &str,
            sender: &ruma::UserId,
            content: serde_json::Value,
        ) -> OwnedEventId {
            let only_prev_is_create = prev.as_ref().is_some_and(|p| {
                self.store
                    .get(p)
                    .is_some_and(|e| e.event_type == "m.room.create")
            });
            let content_obj =
                to_canonical_object(&content, self.rules.strict_canonical_json).unwrap();
            let incoming = auth::IncomingEvent::new(
                event_type,
                sender,
                Some(&self.room_id),
                Some(state_key),
                &content_obj,
            );
            let expected = auth::expected_auth_types(&incoming, &self.rules).unwrap_or_default();
            let auth_events: Vec<OwnedEventId> = expected
                .iter()
                .filter_map(|key| state.get(key).cloned())
                .collect();
            let id = self.id();
            let event = ResolutionEvent {
                event_id: id.clone(),
                room_id: self.room_id.clone(),
                event_type: event_type.to_owned(),
                state_key: state_key.to_owned(),
                sender: sender.to_owned(),
                content: content_obj,
                depth,
                origin_server_ts: depth,
                auth_events,
                prev_events: prev.into_iter().collect(),
                only_prev_event_is_room_create: only_prev_is_create,
            };
            self.store.insert(id.clone(), event);
            state.insert((event_type.to_owned(), state_key.to_owned()), id.clone());
            id
        }

        fn genesis(&mut self) -> (StateMap, OwnedEventId, OwnedUserId, OwnedUserId) {
            let creator = UserId::parse("@creator:hs1").unwrap();
            let member = UserId::parse("@member:hs2").unwrap();
            let mut state = StateMap::new();
            let create = self.push(
                &mut state,
                None,
                1,
                "m.room.create",
                "",
                &creator,
                json!({"creator": creator.as_str()}),
            );
            let join = self.push(
                &mut state,
                Some(create),
                2,
                "m.room.member",
                creator.as_str(),
                &creator,
                json!({"membership": "join"}),
            );
            let pl = self.push(
                &mut state,
                Some(join),
                3,
                "m.room.power_levels",
                "",
                &creator,
                json!({
                    "users": {creator.as_str(): 100},
                    "ban": 50, "kick": 50, "redact": 50, "invite": 0,
                    "users_default": 0, "events_default": 0, "state_default": 50,
                }),
            );
            let jr = self.push(
                &mut state,
                Some(pl),
                4,
                "m.room.join_rules",
                "",
                &creator,
                json!({"join_rule": "public"}),
            );
            let tip = self.push(
                &mut state,
                Some(jr),
                5,
                "m.room.member",
                member.as_str(),
                &member,
                json!({"membership": "join"}),
            );
            (state, tip, creator, member)
        }
    }

    #[test]
    fn no_conflict_returns_union() {
        let mut b = Builder::new();
        let (state, _tip, _creator, _member) = b.genesis();
        let resolved = resolve(&b.rules, &[state.clone(), state.clone()], &b.store).unwrap();
        assert_eq!(resolved, state);
    }

    #[test]
    fn power_levels_conflict_resolved_by_auth_not_just_depth() {
        let mut b = Builder::new();
        let (base, tip, creator, member) = b.genesis();

        // Branch A: creator (power 100) legitimately raises their own bookkeeping (no-op change,
        // still valid) at depth 6.
        let mut state_a = base.clone();
        let pl_a = b.push(
            &mut state_a,
            Some(tip.clone()),
            6,
            "m.room.power_levels",
            "",
            &creator,
            json!({
                "users": {creator.as_str(): 100},
                "ban": 60, "kick": 50, "redact": 50, "invite": 0,
                "users_default": 0, "events_default": 0, "state_default": 50,
            }),
        );
        state_a.insert(("m.room.power_levels".to_owned(), String::new()), pl_a);

        // Branch B: member (power 0) illegitimately tries to change power levels at a *higher*
        // depth than branch A's event, so naive depth-only ordering would prefer it.
        let mut state_b = base.clone();
        let bogus = b.push(
            &mut state_b,
            Some(tip),
            7,
            "m.room.power_levels",
            "",
            &member,
            json!({
                "users": {creator.as_str(): 100, member.as_str(): 100},
                "ban": 50, "kick": 50, "redact": 50, "invite": 0,
                "users_default": 0, "events_default": 0, "state_default": 50,
            }),
        );
        state_b.insert(
            ("m.room.power_levels".to_owned(), String::new()),
            bogus.clone(),
        );

        let resolved = resolve(&b.rules, &[state_a, state_b], &b.store).unwrap();
        let winner = resolved
            .get(&("m.room.power_levels".to_owned(), String::new()))
            .unwrap();
        assert_ne!(
            *winner, bogus,
            "the unauthorized power_levels change must not win"
        );
    }

    #[test]
    fn generic_conflict_picks_highest_depth_passing_candidate() {
        let mut b = Builder::new();
        let (base, tip, creator, _member) = b.genesis();

        let mut state_a = base.clone();
        let topic_a = b.push(
            &mut state_a,
            Some(tip.clone()),
            6,
            "m.room.topic",
            "",
            &creator,
            json!({"topic": "first"}),
        );
        state_a.insert(("m.room.topic".to_owned(), String::new()), topic_a.clone());

        let mut state_b = base.clone();
        let topic_b = b.push(
            &mut state_b,
            Some(tip),
            7,
            "m.room.topic",
            "",
            &creator,
            json!({"topic": "second"}),
        );
        state_b.insert(("m.room.topic".to_owned(), String::new()), topic_b.clone());

        let resolved = resolve(&b.rules, &[state_a, state_b], &b.store).unwrap();
        let winner = resolved
            .get(&("m.room.topic".to_owned(), String::new()))
            .unwrap();
        // Both are sent by the creator and both pass auth; the higher-depth one wins.
        assert_eq!(*winner, topic_b);
    }
}
