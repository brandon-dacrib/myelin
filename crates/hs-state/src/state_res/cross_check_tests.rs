//! Cross-implementation property tests: [`super::v2`] (which delegates to `ruma-state-res`) and
//! [`super::oracle`] (independent, written straight from the spec) must agree on random forked
//! DAGs.
//!
//! `docs/workstreams/02-state-and-model.md`: "an independent oracle implementation of v2 and v2.1
//! ... and cross-implementation property tests over randomly generated DAGs."

use std::collections::BTreeMap;

use hs_model::room_version;
use proptest::prelude::*;

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
