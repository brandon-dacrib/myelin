//! Cross-implementation property tests: [`super::v2`] (which delegates to `ruma-state-res`) and
//! [`super::oracle`] (independent, written straight from the spec) must agree on random forked
//! DAGs.
//!
//! `docs/workstreams/02-state-and-model.md`: "an independent oracle implementation of v2 and v2.1
//! ... and cross-implementation property tests over randomly generated DAGs."

use std::collections::BTreeMap;

use hs_model::canonical::to_canonical_object;
use hs_model::room_version::{self, RoomVersionRules};
use proptest::prelude::*;
use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UserId};
use serde_json::json;

use super::{EventStore, ResolutionEvent, StateMap};
use crate::auth;

/// Builds a room's event store branch by branch, computing realistic `auth_events` for each new
/// event via [`auth::expected_auth_types`] resolved against that branch's running state.
struct RoomBuilder {
    room_id: OwnedRoomId,
    rules: RoomVersionRules,
    store: EventStore,
    next_n: u32,
    next_ts: i64,
}

impl RoomBuilder {
    fn new(rules: RoomVersionRules) -> Self {
        Self {
            room_id: RoomId::parse("!room:hs1").unwrap(),
            rules,
            store: EventStore::new(),
            next_n: 0,
            next_ts: 0,
        }
    }

    fn event_id(&mut self) -> OwnedEventId {
        self.next_n += 1;
        OwnedEventId::try_from(format!("$e{}:hs1", self.next_n).as_str()).unwrap()
    }

    /// Appends an event to `branch_state` (the running state map for one branch/fork), computing
    /// its `auth_events` from that state, and returns its ID. `prev` is the tip event of this
    /// branch before this one.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        branch_state: &mut StateMap,
        prev: Option<OwnedEventId>,
        depth: i64,
        event_type: &str,
        state_key: &str,
        sender: &UserId,
        content: serde_json::Value,
    ) -> OwnedEventId {
        let only_prev_is_create = prev.as_ref().is_some_and(|p| {
            self.store
                .get(p)
                .is_some_and(|e| e.event_type == "m.room.create")
        });
        let content_obj = to_canonical_object(&content, self.rules.strict_canonical_json).unwrap();
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
            .filter_map(|key| branch_state.get(key).cloned())
            .collect();

        let id = self.event_id();
        self.next_ts += 1;
        let event = ResolutionEvent {
            event_id: id.clone(),
            room_id: self.room_id.clone(),
            event_type: event_type.to_owned(),
            state_key: state_key.to_owned(),
            sender: sender.to_owned(),
            content: content_obj,
            depth,
            origin_server_ts: self.next_ts,
            auth_events,
            prev_events: prev.into_iter().collect(),
            only_prev_event_is_room_create: only_prev_is_create,
        };
        self.store.insert(id.clone(), event);
        branch_state.insert((event_type.to_owned(), state_key.to_owned()), id.clone());
        id
    }

    /// Builds the common genesis: create, creator join, power levels, join rules, and two other
    /// members already joined. Returns the base state map and the tip event ID.
    fn genesis(&mut self) -> (StateMap, OwnedEventId, [OwnedUserId; 3]) {
        let creator = UserId::parse("@creator:hs1").unwrap();
        let alice = UserId::parse("@alice:hs2").unwrap();
        let bob = UserId::parse("@bob:hs3").unwrap();

        let mut state = StateMap::new();
        let create_content = if self.rules.use_room_create_sender {
            json!({})
        } else {
            json!({"creator": creator.as_str()})
        };
        let create_id = self.push(
            &mut state,
            None,
            1,
            "m.room.create",
            "",
            &creator,
            create_content,
        );
        let join_id = self.push(
            &mut state,
            Some(create_id.clone()),
            2,
            "m.room.member",
            creator.as_str(),
            &creator,
            json!({"membership": "join"}),
        );
        let pl_id = self.push(
            &mut state,
            Some(join_id),
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
        let jr_id = self.push(
            &mut state,
            Some(pl_id),
            4,
            "m.room.join_rules",
            "",
            &creator,
            json!({"join_rule": "public"}),
        );
        let alice_id = self.push(
            &mut state,
            Some(jr_id.clone()),
            5,
            "m.room.member",
            alice.as_str(),
            &alice,
            json!({"membership": "join"}),
        );
        let tip = self.push(
            &mut state,
            Some(alice_id),
            6,
            "m.room.member",
            bob.as_str(),
            &bob,
            json!({"membership": "join"}),
        );
        (state, tip, [creator, alice, bob])
    }
}

/// One divergent action applied on top of the shared genesis.
#[derive(Debug, Clone)]
enum Action {
    Kick { who: u8, by: u8 },
    Ban { who: u8, by: u8 },
    RaisePower { who: u8, level: i64, by: u8 },
    ChangeJoinRule { rule: &'static str, by: u8 },
}

fn action_strategy() -> impl Strategy<Value = Action> {
    prop_oneof![
        (0u8..3, 0u8..3).prop_map(|(who, by)| Action::Kick { who, by }),
        (0u8..3, 0u8..3).prop_map(|(who, by)| Action::Ban { who, by }),
        (0u8..3, 0i64..100, 0u8..3).prop_map(|(who, level, by)| Action::RaisePower {
            who,
            level,
            by
        }),
        (prop_oneof![Just("public"), Just("invite")], 0u8..3)
            .prop_map(|(rule, by)| Action::ChangeJoinRule { rule, by }),
    ]
}

/// Applies a short sequence of actions on top of `state`/`tip`, returning the resulting state map.
fn apply_branch(
    builder: &mut RoomBuilder,
    users: &[OwnedUserId; 3],
    mut state: StateMap,
    mut tip: OwnedEventId,
    mut depth: i64,
    actions: &[Action],
) -> StateMap {
    for action in actions {
        depth += 1;
        let (event_type, state_key, sender, content) = match action {
            Action::Kick { who, by } => (
                "m.room.member",
                users[*who as usize % 3].as_str().to_owned(),
                users[*by as usize % 3].clone(),
                json!({"membership": "leave"}),
            ),
            Action::Ban { who, by } => (
                "m.room.member",
                users[*who as usize % 3].as_str().to_owned(),
                users[*by as usize % 3].clone(),
                json!({"membership": "ban"}),
            ),
            Action::RaisePower { who, level, by } => (
                "m.room.power_levels",
                String::new(),
                users[*by as usize % 3].clone(),
                json!({
                    "users": {users[*who as usize % 3].as_str(): level, users[0].as_str(): 100},
                    "ban": 50, "kick": 50, "redact": 50, "invite": 0,
                    "users_default": 0, "events_default": 0, "state_default": 50,
                }),
            ),
            Action::ChangeJoinRule { rule, by } => (
                "m.room.join_rules",
                String::new(),
                users[*by as usize % 3].clone(),
                json!({"join_rule": rule}),
            ),
        };
        tip = builder.push(
            &mut state,
            Some(tip.clone()),
            depth,
            event_type,
            &state_key,
            &sender,
            content,
        );
    }
    state
}

fn versions() -> Vec<RoomVersionId> {
    vec![
        RoomVersionId::V2,
        RoomVersionId::V6,
        RoomVersionId::V8,
        RoomVersionId::V11,
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn oracle_agrees_with_ruma_backed_v2_on_random_forks(
        version_idx in 0usize..4,
        actions_a in prop::collection::vec(action_strategy(), 0..3),
        actions_b in prop::collection::vec(action_strategy(), 0..3),
    ) {
        let version = versions()[version_idx].clone();
        let rules = room_version::rules_for(&version).unwrap();
        let mut builder = RoomBuilder::new(rules);
        let (base_state, tip, users) = builder.genesis();

        let state_a = apply_branch(&mut builder, &users, base_state.clone(), tip.clone(), 6, &actions_a);
        let state_b = apply_branch(&mut builder, &users, base_state, tip, 6, &actions_b);

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
