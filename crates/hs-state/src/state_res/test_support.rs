//! Shared test-only machinery for building small forked rooms and driving them through this
//! crate's various resolution paths.
//!
//! Originally lived entirely inside [`super::cross_check_tests`] (the oracle-vs-`ruma-state-res`
//! property test); pulled out so [`super::fork_production_cross_check`] (the production
//! `StateStore` vs. both reference implementations property test) can build the exact same kind
//! of forked room and replay it through [`crate::kv_store::KvStateStore`] as well, rather than
//! duplicating [`RoomBuilder`].

use hs_model::canonical::to_canonical_object;
use hs_model::room_version::RoomVersionRules;
use proptest::prelude::*;
use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UserId};
use serde_json::json;

use super::{EventStore, ResolutionEvent, StateMap};
use crate::auth;

/// Builds a room's event store branch by branch, computing realistic `auth_events` for each new
/// event via [`auth::expected_auth_types`] resolved against that branch's running state.
pub(crate) struct RoomBuilder {
    pub(crate) room_id: OwnedRoomId,
    pub(crate) rules: RoomVersionRules,
    pub(crate) store: EventStore,
    next_n: u32,
    next_ts: i64,
    /// Every event pushed, in push (causal) order: parents always appear before their children.
    /// [`super::fork_production_cross_check`] replays events into
    /// [`crate::kv_store::KvStateStore`] in this order so each event's `auth_events`/`prev_events`
    /// are always already known to the store.
    pub(crate) order: Vec<OwnedEventId>,
}

impl RoomBuilder {
    pub(crate) fn new(rules: RoomVersionRules) -> Self {
        Self {
            room_id: RoomId::parse("!room:hs1").unwrap(),
            rules,
            store: EventStore::new(),
            next_n: 0,
            next_ts: 0,
            order: Vec::new(),
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
    pub(crate) fn push(
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
        self.order.push(id.clone());
        id
    }

    /// Builds the common genesis: create, creator join, power levels, join rules, and two other
    /// members already joined. Returns the base state map and the tip event ID.
    pub(crate) fn genesis(&mut self) -> (StateMap, OwnedEventId, [OwnedUserId; 3]) {
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
pub(crate) enum Action {
    Kick { who: u8, by: u8 },
    Ban { who: u8, by: u8 },
    RaisePower { who: u8, level: i64, by: u8 },
    ChangeJoinRule { rule: &'static str, by: u8 },
}

pub(crate) fn action_strategy() -> impl Strategy<Value = Action> {
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

/// Applies a short sequence of actions on top of `state`/`tip`, returning the resulting state map
/// and the branch's new tip event ID.
pub(crate) fn apply_branch(
    builder: &mut RoomBuilder,
    users: &[OwnedUserId; 3],
    mut state: StateMap,
    mut tip: OwnedEventId,
    mut depth: i64,
    actions: &[Action],
) -> (StateMap, OwnedEventId) {
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
    (state, tip)
}

pub(crate) fn versions() -> Vec<RoomVersionId> {
    vec![
        RoomVersionId::V2,
        RoomVersionId::V6,
        RoomVersionId::V8,
        RoomVersionId::V11,
    ]
}
