//! The three-way fork cross-check: a genuine fork (two branches setting conflicting state off one
//! shared parent) resolved through the production [`crate::kv_store::ProductionStateStore`] must
//! agree, key by key, with both [`super::v2::resolve`] (backed by `ruma-state-res`) and
//! [`super::oracle::resolve`] (this crate's independent, spec-derived implementation) resolving
//! the *same* two conflicting state maps.
//!
//! This is stronger evidence than `kv_store::tests::candidate_b_frames`'s fork-and-merge test:
//! that test checks the production representation against one hardcoded expected winner, which
//! only proves the plumbing moves an event through correctly. This module checks it against two
//! independent implementations of the algorithm itself, so a real state-resolution bug (not just a
//! wiring bug) would show up as a three-way disagreement here.
//!
//! Lives inside the crate, not under `crates/hs-state/tests/`, because [`super::oracle`] is
//! `#[cfg(test)] pub(crate)` (see `super::oracle`'s module docs for why it stays test-only) and is
//! therefore invisible to an external integration test.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use hs_kv::memory::MemoryBackend;
use hs_model::ids::EventSn;
use hs_model::room_version::{self, RoomVersionRules};
use proptest::prelude::*;
use ruma::{OwnedEventId, RoomVersionId};

use super::test_support::{Action, RoomBuilder, action_strategy, apply_branch, versions};
use super::{EventStore, StateMap};
use crate::api::StateStore;
use crate::frames::FrameRepr;
use crate::kv_store::KvStateStore;

/// The store type + the event-id <-> `EventSn` mapping used to build it: what
/// [`replay_into_production_store`] returns.
type ReplayedStore = (
    KvStateStore<FrameRepr<MemoryBackend>>,
    BTreeMap<OwnedEventId, EventSn>,
    BTreeMap<EventSn, OwnedEventId>,
);

/// Replays every event `builder` has built, in push (causal) order, into a fresh production
/// [`KvStateStore`] over an in-memory backend, returning the store plus the event-id <-> `EventSn`
/// mapping used to feed it (needed to translate the reference implementations' `OwnedEventId`
/// winners into what [`StateStore::get`] returns, and back).
fn replay_into_production_store(
    room_version: &RoomVersionId,
    order: &[OwnedEventId],
    events: &EventStore,
) -> ReplayedStore {
    let repr =
        FrameRepr::new(MemoryBackend::default()).expect("in-memory backend never fails to open");
    let store = KvStateStore::new(room_version.clone(), repr)
        .expect("RoomBuilder only ever builds rooms of a room version this crate supports");

    let mut sn_of: BTreeMap<OwnedEventId, EventSn> = BTreeMap::new();
    let mut id_of: BTreeMap<EventSn, OwnedEventId> = BTreeMap::new();

    for (i, id) in order.iter().enumerate() {
        // 1-based so EventSn(0) is never assigned, matching the convention `kv_store`'s own tests
        // use.
        let sn = EventSn::new((i + 1) as u64);
        let event = events
            .get(id)
            .expect("RoomBuilder's `order` tracks every event it ever pushed");

        let auth_events: Vec<EventSn> = event.auth_events.iter().map(|a| sn_of[a]).collect();
        let prev_events: Vec<EventSn> = event.prev_events.iter().map(|p| sn_of[p]).collect();

        store
            .add_event(
                sn,
                id.clone(),
                event.room_id.clone(),
                &event.event_type,
                Some(&event.state_key),
                event.sender.clone(),
                event.content.clone(),
                event.depth,
                event.origin_server_ts,
                &auth_events,
                &prev_events,
                event.only_prev_event_is_room_create,
            )
            .expect(
                "RoomBuilder only produces well-formed events replayed in causal (parents-first) \
                 order",
            );

        sn_of.insert(id.clone(), sn);
        id_of.insert(sn, id.clone());
    }

    (store, sn_of, id_of)
}

/// Resolves `state_a`/`state_b` via both reference implementations (asserting they already agree
/// with each other -- if they don't, that is a bug in this test's setup or in one of the two
/// reference implementations, not something the production store's involvement could explain) and
/// via the production store (built by replaying `builder`'s events and resolving `tip_a`'s and
/// `tip_b`'s states), then compares every key either branch or either reference implementation
/// ever set. Returns `Err` describing the first disagreement found, if any.
#[allow(clippy::too_many_arguments)]
fn check_fork(
    room_version: &RoomVersionId,
    rules: &RoomVersionRules,
    builder: &RoomBuilder,
    state_a: &StateMap,
    state_b: &StateMap,
    tip_a: &OwnedEventId,
    tip_b: &OwnedEventId,
) -> Result<(), String> {
    let states = vec![state_a.clone(), state_b.clone()];

    let oracle_result = super::oracle::resolve(rules, &states, &builder.store)
        .map_err(|e| format!("state_res::oracle::resolve failed: {e}"))?;
    let v2_result = super::v2::resolve(room_version, &states, &builder.store)
        .map_err(|e| format!("state_res::v2::resolve (ruma-state-res) failed: {e}"))?;

    if oracle_result != v2_result {
        return Err(format!(
            "the oracle and ruma-state-res already disagree with each other, before the \
             production store is even involved -- oracle={oracle_result:?} ruma={v2_result:?}"
        ));
    }

    let (store, sn_of, id_of) =
        replay_into_production_store(room_version, &builder.order, &builder.store);

    let root_a = store
        .state_at(sn_of[tip_a])
        .map_err(|e| format!("production state_at(tip_a) failed: {e}"))?;
    let root_b = store
        .state_at(sn_of[tip_b])
        .map_err(|e| format!("production state_at(tip_b) failed: {e}"))?;
    let merged = store
        .resolve(room_version, &[root_a, root_b])
        .map_err(|e| format!("production resolve() failed: {e}"))?;

    // Every key either branch, or either reference resolution, ever touched -- state resolution
    // never invents a key absent from every input, so this superset is exhaustive.
    let mut all_keys: BTreeSet<(String, String)> = state_a.keys().cloned().collect();
    all_keys.extend(state_b.keys().cloned());
    all_keys.extend(oracle_result.keys().cloned());

    for key in &all_keys {
        let key_id = store
            .intern_state_key(&key.0, &key.1)
            .map_err(|e| format!("intern_state_key({key:?}) failed: {e}"))?;
        let got_sn = store
            .get(merged, key_id)
            .map_err(|e| format!("production get({key:?}) failed: {e}"))?;
        let got_id = got_sn.map(|sn| id_of[&sn].clone());
        let expected = oracle_result.get(key).cloned();

        if got_id != expected {
            return Err(format!(
                "production store disagrees with the oracle/ruma-state-res agreement on \
                 {key:?}: production={got_id:?} oracle==ruma={expected:?}"
            ));
        }
    }

    Ok(())
}

/// A hand-built fork exercising two different auth paths at once: branch A raises alice's power
/// level and then kicks bob; branch B raises bob's power level (a different, conflicting
/// `m.room.power_levels` event) and bans bob instead (a different, conflicting `m.room.member`
/// event for the same state key). Resolving the two branches must pick one winner per key, and
/// the production store must agree with both reference implementations on which.
#[test]
fn production_store_matches_oracle_and_ruma_on_power_levels_and_membership_fork() {
    let version = RoomVersionId::V11;
    let rules = room_version::rules_for(&version).unwrap();
    let mut builder = RoomBuilder::new(rules);
    let (base_state, tip, users) = builder.genesis();

    // who/by indices: 0 = creator, 1 = alice, 2 = bob (see `RoomBuilder::genesis`).
    let actions_a = vec![
        Action::RaisePower {
            who: 1,
            level: 60,
            by: 0,
        },
        Action::Kick { who: 2, by: 0 },
    ];
    let actions_b = vec![
        Action::RaisePower {
            who: 2,
            level: 75,
            by: 0,
        },
        Action::Ban { who: 2, by: 0 },
    ];

    let (state_a, tip_a) = apply_branch(
        &mut builder,
        &users,
        base_state.clone(),
        tip.clone(),
        6,
        &actions_a,
    );
    let (state_b, tip_b) = apply_branch(&mut builder, &users, base_state, tip, 6, &actions_b);

    let power_levels_key = ("m.room.power_levels".to_owned(), String::new());
    let bob_member_key = ("m.room.member".to_owned(), users[2].as_str().to_owned());
    assert_ne!(
        state_a.get(&power_levels_key),
        state_b.get(&power_levels_key),
        "the two branches must disagree on power_levels for this to be a genuine fork"
    );
    assert_ne!(
        state_a.get(&bob_member_key),
        state_b.get(&bob_member_key),
        "the two branches must disagree on bob's membership for this to be a genuine fork"
    );

    if let Err(message) = check_fork(
        &version, &rules, &builder, &state_a, &state_b, &tip_a, &tip_b,
    ) {
        panic!("{message}");
    }
}

// The property-test version of the above: random forks (mixing membership and power-level
// actions, via the same `Action` generator `cross_check_tests` uses) across several room
// versions, replayed through the production store and checked against both reference
// implementations. A single hand-built fork proves the plumbing; this is what would actually
// catch a divergence between the production representation and the two independent algorithms.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn production_store_matches_oracle_and_ruma_on_random_forks(
        version_idx in 0usize..5,
        actions_a in prop::collection::vec(action_strategy(), 0..3),
        actions_b in prop::collection::vec(action_strategy(), 0..3),
    ) {
        let version = versions()[version_idx].clone();
        let rules = room_version::rules_for(&version).unwrap();
        let mut builder = RoomBuilder::new(rules);
        let (base_state, tip, users) = builder.genesis();

        let (state_a, tip_a) =
            apply_branch(&mut builder, &users, base_state.clone(), tip.clone(), 6, &actions_a);
        let (state_b, tip_b) =
            apply_branch(&mut builder, &users, base_state, tip, 6, &actions_b);

        let result = check_fork(&version, &rules, &builder, &state_a, &state_b, &tip_a, &tip_b);
        prop_assert!(
            result.is_ok(),
            "fork mismatch for room version {} with actions {:?} / {:?}: {}",
            version.as_str(),
            actions_a,
            actions_b,
            result.unwrap_err()
        );
    }
}
