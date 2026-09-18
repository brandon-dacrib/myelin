//! An independent implementation of state resolution v2 and v2.1, written straight from the
//! spec, used only as a test oracle to cross-check `v2` (which delegates to `ruma-state-res`).
//!
//! `docs/workstreams/02-state-and-model.md`: "an independent oracle implementation of v2 and v2.1
//! straight from the spec, used only in tests."
//!
//! Re-expressed from the "State resolution" section of the room version 2 specification page
//! (`refs/matrix-spec/content/rooms/fragments/v2-state-res.md`) and the v2.1 changes described on
//! the room version 12 page (`refs/matrix-spec/content/rooms/v12.md#state-resolution`), both
//! Apache-2.0. This module intentionally does not look at `super::v2` or `ruma-state-res` at all:
//! the point of an oracle is that it was derived independently, so that when it agrees with the
//! `ruma-state-res`-backed implementation the agreement is evidence about the *specification*,
//! not just about this crate's own consistency.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use hs_model::canonical::CanonicalJsonValue;
use hs_model::power_levels::PowerLevels;
use hs_model::room_version::{RoomVersionRules, StateResolutionVersion};
use ruma::OwnedEventId;

use super::{EventStore, ResolutionEvent, StateMap, incoming_event, split_conflicted};
use crate::auth;
use crate::error::StateResError;
use crate::state_fetch::{StateEntry, StateFetch};

type Id = OwnedEventId;

/// Resolves a set of state maps under state resolution v2/v2.1, straight from the spec text (see
/// the module docs).
///
/// # Errors
/// Returns [`StateResError::UnsupportedInput`] if `rules` does not use state resolution v2/v2.1,
/// or [`StateResError::MissingEvent`] if a candidate event is not present in `store`.
pub fn resolve(
    rules: &RoomVersionRules,
    states: &[StateMap],
    store: &EventStore,
) -> Result<StateMap, StateResError> {
    let v2_1 = match rules.state_res {
        StateResolutionVersion::V2 { v2_1 } => v2_1,
        StateResolutionVersion::V1 => {
            return Err(StateResError::UnsupportedInput(
                "room version uses state resolution v1, not v2".to_owned(),
            ));
        }
    };

    let (unconflicted, conflicted) = split_conflicted(states);
    let conflicted_ids: HashSet<Id> = conflicted.values().flatten().cloned().collect();
    if conflicted_ids.is_empty() {
        return Ok(unconflicted);
    }

    for id in conflicted_ids.iter().chain(unconflicted.values()) {
        if !store.contains_key(id) {
            return Err(StateResError::MissingEvent(id.to_string()));
        }
    }

    let mut cache: HashMap<Id, HashSet<Id>> = HashMap::new();

    let auth_chains: Vec<HashSet<Id>> = states
        .iter()
        .map(|state| {
            let mut set = HashSet::new();
            for id in state.values() {
                set.extend(auth_chain_closure(id, store, &mut cache));
            }
            set
        })
        .collect();
    let auth_diff = auth_difference(&auth_chains);

    let subgraph = if v2_1 {
        conflicted_state_subgraph(&conflicted_ids, store, &mut cache)
    } else {
        HashSet::new()
    };

    let mut full_conflicted: HashSet<Id> = conflicted_ids.clone();
    full_conflicted.extend(auth_diff);
    full_conflicted.extend(subgraph);

    // Step 1: X = power events in the full conflicted set, enlarged with their in-set auth-chain
    // ancestors, ordered by reverse topological power ordering.
    let mut x: HashSet<Id> = HashSet::new();
    for id in &full_conflicted {
        let Some(event) = store.get(id) else { continue };
        if is_power_event(event) {
            x.insert(id.clone());
            for ancestor in auth_chain_closure(id, store, &mut cache) {
                if full_conflicted.contains(&ancestor) {
                    x.insert(ancestor);
                }
            }
        }
    }
    let ordered_x = reverse_topological_power_order(&x, store, rules);

    // Step 2: iterative auth checks from the unconflicted state map (v2) or an empty map (v2.1).
    let base = if v2_1 {
        StateMap::new()
    } else {
        unconflicted.clone()
    };
    let partial = iterative_auth_checks(rules, &base, &ordered_x, store);

    // Step 3: remaining events (full conflicted set minus X), ordered by mainline ordering based
    // on the power-levels event in the partially resolved state.
    let remaining: HashSet<Id> = full_conflicted.difference(&x).cloned().collect();
    let mainline = mainline_for(&partial, store);
    let mut ordered_remaining: Vec<Id> = remaining.into_iter().collect();
    ordered_remaining.sort_by(|a, b| mainline_cmp(a, b, &mainline, store));

    // Step 4: iterative auth checks on the partial state and the remaining events.
    let mut resolved = iterative_auth_checks(rules, &partial, &ordered_remaining, store);

    // Step 5: unconflicted entries win over anything resolution produced for the same key.
    for (key, id) in &unconflicted {
        resolved.insert(key.clone(), id.clone());
    }

    Ok(resolved)
}

fn mainline_for(partial: &StateMap, store: &EventStore) -> Vec<Id> {
    match partial.get(&("m.room.power_levels".to_owned(), String::new())) {
        Some(p0) => {
            let mut mainline = vec![p0.clone()];
            mainline.extend(power_levels_chain(p0, store));
            mainline
        }
        None => Vec::new(),
    }
}

/// A *power event*: `m.room.power_levels`, `m.room.join_rules`, or an `m.room.member` leave/ban
/// sent by someone other than its target.
fn is_power_event(event: &ResolutionEvent) -> bool {
    match event.event_type.as_str() {
        "m.room.power_levels" | "m.room.join_rules" => true,
        "m.room.member" => {
            let membership = event
                .content
                .get("membership")
                .and_then(CanonicalJsonValue::as_str);
            matches!(membership, Some("leave") | Some("ban"))
                && event.sender.as_str() != event.state_key
        }
        _ => false,
    }
}

/// The auth chain of `id`: every event reachable by following `auth_events`, transitively,
/// including `id`'s own direct auth events but not `id` itself. Memoized in `cache`.
fn auth_chain_closure(
    id: &Id,
    store: &EventStore,
    cache: &mut HashMap<Id, HashSet<Id>>,
) -> HashSet<Id> {
    if let Some(cached) = cache.get(id) {
        return cached.clone();
    }
    let mut result = HashSet::new();
    if let Some(event) = store.get(id) {
        for auth_id in &event.auth_events {
            if result.insert(auth_id.clone()) {
                let sub = auth_chain_closure(auth_id, store, cache);
                result.extend(sub);
            }
        }
    }
    cache.insert(id.clone(), result.clone());
    result
}

/// The auth difference: events that appear in the union of every input state map's auth chain but
/// not in the intersection, i.e. not present in *every* chain.
fn auth_difference(chains: &[HashSet<Id>]) -> HashSet<Id> {
    let mut counts: HashMap<Id, usize> = HashMap::new();
    for chain in chains {
        for id in chain {
            *counts.entry(id.clone()).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count < chains.len())
        .map(|(id, _)| id)
        .collect()
}

/// The conflicted state subgraph (v2.1 / room version 12 onward): every event lying on some
/// `auth_events` path between two members of the conflicted state set.
fn conflicted_state_subgraph(
    conflicted_ids: &HashSet<Id>,
    store: &EventStore,
    cache: &mut HashMap<Id, HashSet<Id>>,
) -> HashSet<Id> {
    let mut subgraph = HashSet::new();
    let closures: HashMap<Id, HashSet<Id>> = conflicted_ids
        .iter()
        .map(|id| (id.clone(), auth_chain_closure(id, store, cache)))
        .collect();

    for a in conflicted_ids {
        let Some(closure_a) = closures.get(a) else {
            continue;
        };
        for b in conflicted_ids {
            if a == b || !closure_a.contains(b) {
                continue;
            }
            subgraph.insert(a.clone());
            subgraph.insert(b.clone());
            for c in closure_a {
                if c == b {
                    continue;
                }
                if auth_chain_closure(c, store, cache).contains(b) {
                    subgraph.insert(c.clone());
                }
            }
        }
    }
    subgraph
}

/// Sorts `events` into the reverse topological power ordering (Kahn's algorithm over the
/// `auth_events` edges restricted to `events`, breaking ties by the power-order comparison
/// relation).
fn reverse_topological_power_order(
    events: &HashSet<Id>,
    store: &EventStore,
    rules: &RoomVersionRules,
) -> Vec<Id> {
    let mut in_degree: HashMap<Id, usize> = events.iter().map(|id| (id.clone(), 0)).collect();
    let mut successors: HashMap<Id, Vec<Id>> = HashMap::new();
    for id in events {
        let Some(event) = store.get(id) else { continue };
        for auth_id in &event.auth_events {
            if events.contains(auth_id) {
                successors
                    .entry(auth_id.clone())
                    .or_default()
                    .push(id.clone());
                *in_degree.get_mut(id).expect("id is in events") += 1;
            }
        }
    }

    let mut ready: Vec<Id> = in_degree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut result = Vec::with_capacity(events.len());

    while !ready.is_empty() {
        ready.sort_by(|a, b| power_order_cmp(a, b, store, rules));
        let next = ready.remove(0);
        if let Some(succs) = successors.get(&next) {
            for succ in succs {
                let degree = in_degree.get_mut(succ).expect("successor is in events");
                *degree -= 1;
                if *degree == 0 {
                    ready.push(succ.clone());
                }
            }
        }
        result.push(next);
    }

    result
}

/// `x < y` if `x`'s sender has *greater* power (per the `m.room.power_levels` event in `x`'s own
/// `auth_events`) than `y`'s, or equal power and earlier `origin_server_ts`, or equal power and
/// timestamp and a lexicographically smaller event ID.
fn power_order_cmp(a: &Id, b: &Id, store: &EventStore, rules: &RoomVersionRules) -> Ordering {
    let pa = power_level_of(a, store, rules);
    let pb = power_level_of(b, store, rules);
    pb.cmp(&pa).then_with(|| tiebreak(a, b, store))
}

fn tiebreak(a: &Id, b: &Id, store: &EventStore) -> Ordering {
    let (Some(ea), Some(eb)) = (store.get(a), store.get(b)) else {
        return a.as_str().cmp(b.as_str());
    };
    ea.origin_server_ts
        .cmp(&eb.origin_server_ts)
        .then_with(|| a.as_str().cmp(b.as_str()))
}

/// The power level of `id`'s sender, per the `m.room.power_levels` event in `id`'s own
/// `auth_events` (not the resolved state); 0 if there is none.
fn power_level_of(id: &Id, store: &EventStore, rules: &RoomVersionRules) -> i64 {
    let Some(event) = store.get(id) else { return 0 };
    let power_levels_event = event.auth_events.iter().find_map(|auth_id| {
        store
            .get(auth_id)
            .filter(|e| e.event_type == "m.room.power_levels")
    });
    match power_levels_event {
        Some(pl) => PowerLevels::parse(&pl.content, rules)
            .map(|p| p.user_power(&event.sender))
            .unwrap_or(0),
        None => 0,
    }
}

/// The chain `[e1, e2, ...]` where `e(j+1)` is the `m.room.power_levels` event in `e_j`'s
/// `auth_events`, starting from `start` (not included in the result).
fn power_levels_chain(start: &Id, store: &EventStore) -> Vec<Id> {
    let mut chain = Vec::new();
    let mut current = start.clone();
    while let Some(event) = store.get(&current) {
        let next = event.auth_events.iter().find(|id| {
            store
                .get(*id)
                .is_some_and(|e| e.event_type == "m.room.power_levels")
        });
        match next {
            Some(n) => {
                chain.push(n.clone());
                current = n.clone();
            }
            None => break,
        }
    }
    chain
}

/// The mainline position of `e`: the index in `mainline` of the first event in `e`'s own
/// power-levels chain that appears in `mainline`, or `usize::MAX` ("infinity") if none does.
fn mainline_position(e: &Id, mainline: &[Id], store: &EventStore) -> usize {
    for id in power_levels_chain(e, store) {
        if let Some(index) = mainline.iter().position(|p| *p == id) {
            return index;
        }
    }
    usize::MAX
}

/// `x < y` if `x`'s mainline position is *greater* than `y`'s (further from the mainline root),
/// or equal positions and earlier `origin_server_ts`, or equal positions and timestamps and a
/// lexicographically smaller event ID.
fn mainline_cmp(a: &Id, b: &Id, mainline: &[Id], store: &EventStore) -> Ordering {
    let pa = mainline_position(a, mainline, store);
    let pb = mainline_position(b, mainline, store);
    pb.cmp(&pa).then_with(|| tiebreak(a, b, store))
}

/// Applies the iterative auth checks algorithm: walks `events` in order, authorizing each against
/// `initial` plus whatever has been accepted so far, falling back to the event's own
/// `auth_events` for any `(type, state_key)` the state does not (yet) have.
fn iterative_auth_checks(
    rules: &RoomVersionRules,
    initial: &StateMap,
    events: &[Id],
    store: &EventStore,
) -> StateMap {
    let mut state = initial.clone();
    for id in events {
        let Some(event) = store.get(id) else { continue };
        let fetch = FallbackFetch {
            primary: &state,
            fallback: &event.auth_events,
            store,
        };
        let incoming = incoming_event(event);
        if auth::check_event_auth(rules, &incoming, &fetch).is_ok() {
            state.insert(
                (event.event_type.clone(), event.state_key.clone()),
                id.clone(),
            );
        }
    }
    state
}

/// A [`StateFetch`] over a [`StateMap`] that falls back to a fixed list of events (an event's own
/// `auth_events`) for any key the primary map lacks, per the iterative auth checks algorithm.
struct FallbackFetch<'a> {
    primary: &'a StateMap,
    fallback: &'a [Id],
    store: &'a EventStore,
}

impl StateFetch for FallbackFetch<'_> {
    fn get(&self, event_type: &str, state_key: &str) -> Option<StateEntry<'_>> {
        if let Some(id) = self
            .primary
            .get(&(event_type.to_owned(), state_key.to_owned()))
            && let Some(event) = self.store.get(id)
        {
            return Some(StateEntry {
                sender: &event.sender,
                content: &event.content,
            });
        }
        for id in self.fallback {
            if let Some(event) = self.store.get(id)
                && event.event_type == event_type
                && event.state_key == state_key
            {
                return Some(StateEntry {
                    sender: &event.sender,
                    content: &event.content,
                });
            }
        }
        None
    }
}
