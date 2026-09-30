//! Cross-implementation property tests: [`super::v2`] (which delegates to `ruma-state-res`) and
//! [`super::oracle`] (independent, written straight from the spec) must agree on random forked
//! DAGs.
//!
//! `docs/workstreams/02-state-and-model.md`: "an independent oracle implementation of v2 and v2.1
//! ... and cross-implementation property tests over randomly generated DAGs."

use std::collections::BTreeMap;

use hs_model::room_version;
use proptest::prelude::*;

use ruma::{OwnedEventId, RoomVersionId, UserId};
use serde_json::json;

use super::StateMap;
use super::test_support::{RoomBuilder, action_strategy, apply_branch, versions};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn oracle_agrees_with_ruma_backed_v2_on_random_forks(
        version_idx in 0usize..5,
        actions_a in prop::collection::vec(action_strategy(), 0..3),
        actions_b in prop::collection::vec(action_strategy(), 0..3),
    ) {
        let version = versions()[version_idx].clone();
        let rules = room_version::rules_for(&version).unwrap();
        let mut builder = RoomBuilder::new(rules);
        let (base_state, tip, users) = builder.genesis();

        let (state_a, _tip_a) = apply_branch(&mut builder, &users, base_state.clone(), tip.clone(), 6, &actions_a);
        let (state_b, _tip_b) = apply_branch(&mut builder, &users, base_state, tip, 6, &actions_b);

        let states = vec![state_a, state_b];
        let ours = super::oracle::resolve(&rules, &states, &builder.store);
        let theirs = super::v2::resolve(&version, &states, &builder.store);

        match (ours, theirs) {
            (Ok(ours), Ok(theirs)) => {
                prop_assert_eq!(
                    ours,
                    normalize(theirs),
                    "resolution mismatch for room version {} with actions {:?} / {:?}",
                    version.as_str(), actions_a, actions_b
                );
            }
            (Err(oe), Err(_te)) => {
                // Both failed (for example, an action produced a malformed power_levels
                // event that neither implementation can parse); agreement on failure is
                // acceptable.
                let _ = oe;
            }
            (ours, theirs) => {
                prop_assert!(
                    false,
                    "one implementation succeeded and the other failed: ours={:?} theirs={:?}",
                    ours, theirs
                );
            }
        }
    }
}

/// `ruma_state_res::resolve`'s output keys carry Ruma's `StateEventType`, which stringifies
/// identically to ours; this just re-keys into our plain-string `StateMap` for comparison.
fn normalize(map: StateMap) -> StateMap {
    map.into_iter().collect::<BTreeMap<_, _>>()
}

/// Two events at the same mainline position are ordered by `origin_server_ts` before event ID:
/// the later one wins. Here bob's leave (`$e10`) forks from a power-levels change that cites
/// bob's rejoin (`$e9`); the IDs sort the other way round (`$e10` < `$e9`), so a resolver that
/// loses the timestamps -- as [`super::v2`]'s adapter did by truncating every real timestamp to
/// `u32::MAX` -- resurrects the join. At real timestamps, and in every version, the leave wins,
/// and the oracle agrees.
#[test]
fn a_later_event_at_the_same_mainline_position_wins_at_real_timestamps() {
    for version in [RoomVersionId::V8, RoomVersionId::V10, RoomVersionId::V11] {
        let rules = room_version::rules_for(&version).unwrap();
        let mut builder = RoomBuilder::new(rules);
        let (mut state, tip, [creator, _alice, bob]) = builder.genesis();
        let creator: &UserId = &creator;
        let bob: &UserId = &bob;

        // On the main line: bob leaves and rejoins, with a topic change in between so that the
        // rejoin is `$e9` and the forked leave below is `$e10`.
        let leave_1 = builder.push(
            &mut state,
            Some(tip),
            7,
            "m.room.member",
            bob.as_str(),
            bob,
            json!({"membership": "leave"}),
        );
        let topic = builder.push(
            &mut state,
            Some(leave_1),
            8,
            "m.room.topic",
            "",
            creator,
            json!({"topic": "between"}),
        );
        let rejoin = builder.push(
            &mut state,
            Some(topic),
            9,
            "m.room.member",
            bob.as_str(),
            bob,
            json!({"membership": "join"}),
        );
        assert_eq!(rejoin.as_str(), "$e9:hs1");

        // Fork: bob leaves on one branch while the creator changes the power levels on the
        // other, both citing the rejoin.
        let mut state_a = state.clone();
        let leave_2 = builder.push(
            &mut state_a,
            Some(rejoin.clone()),
            10,
            "m.room.member",
            bob.as_str(),
            bob,
            json!({"membership": "leave"}),
        );
        assert_eq!(leave_2.as_str(), "$e10:hs1");
        let mut state_b = state.clone();
        let levels = builder.push(
            &mut state_b,
            Some(rejoin.clone()),
            10,
            "m.room.power_levels",
            "",
            creator,
            json!({
                "users": {creator.as_str(): 100, bob.as_str(): 50},
                "ban": 50, "kick": 50, "redact": 50, "invite": 50,
                "users_default": 0, "events_default": 0, "state_default": 50,
            }),
        );

        let states = vec![state_a, state_b];
        let member_key = ("m.room.member".to_owned(), bob.to_string());
        let levels_key = ("m.room.power_levels".to_owned(), String::new());
        let ruma_backed = super::v2::resolve(&version, &states, &builder.store).unwrap();
        assert_eq!(
            ruma_backed.get(&member_key),
            Some(&leave_2),
            "version {version}: the later leave must win over the join it superseded"
        );
        assert_eq!(ruma_backed.get(&levels_key), Some(&levels));
        let oracle = super::oracle::resolve(&rules, &states, &builder.store).unwrap();
        assert_eq!(
            normalize(ruma_backed),
            oracle,
            "version {version}: oracle disagrees"
        );
        let _: Option<&OwnedEventId> = oracle.get(&member_key);
    }
}
