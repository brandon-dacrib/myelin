//! Cross-checks `hs_state::auth` against `ruma_state_res::event_auth` on randomly generated
//! events over a small, hand-built room, across every stable room version.
//!
//! `docs/workstreams/02-state-and-model.md`, day-one work: "Event auth for versions 1 to 12
//! written from the spec text, cross-checked against `ruma-state-res`'s auth implementation on
//! random events."
//!
//! The adapter in this file (`TestEvent`, implementing `ruma_state_res::Event`) exists only for
//! this comparison; `hs_state::auth` itself has no dependency on `ruma-state-res` or its `Event`
//! trait.

use std::collections::HashMap;

use hs_model::canonical::{CanonicalJsonObject, to_canonical_object};
use hs_model::room_version::{self, RoomVersionRules};
use hs_state::auth::{self, AuthEventRef, IncomingEvent};
use hs_state::state_fetch::FlatState;
use proptest::prelude::*;
use ruma::events::{StateEventType, TimelineEventType};
use ruma::state_res::Event as RumaEvent;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId,
    RoomVersionId, UInt, UserId,
};
use serde_json::json;
use serde_json::value::RawValue as RawJsonValue;

/// A minimal event adapter for `ruma_state_res::Event`.
#[derive(Clone)]
struct TestEvent {
    id: OwnedEventId,
    room_id: OwnedRoomId,
    sender: OwnedUserId,
    event_type: TimelineEventType,
    content: Box<RawJsonValue>,
    state_key: Option<String>,
    prev_events: Vec<OwnedEventId>,
    auth_events: Vec<OwnedEventId>,
    redacts: Option<OwnedEventId>,
    rejected: bool,
}

impl RumaEvent for TestEvent {
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
        MilliSecondsSinceUnixEpoch(UInt::from(1u32))
    }

    fn event_type(&self) -> &TimelineEventType {
        &self.event_type
    }

    fn content(&self) -> &RawJsonValue {
        &self.content
    }

    fn state_key(&self) -> Option<&str> {
        self.state_key.as_deref()
    }

    fn prev_events(&self) -> Box<dyn DoubleEndedIterator<Item = &Self::Id> + '_> {
        Box::new(self.prev_events.iter())
    }

    fn auth_events(&self) -> Box<dyn DoubleEndedIterator<Item = &Self::Id> + '_> {
        Box::new(self.auth_events.iter())
    }

    fn redacts(&self) -> Option<&Self::Id> {
        self.redacts.as_ref()
    }

    fn rejected(&self) -> bool {
        self.rejected
    }
}

fn event_id(n: u32, server: &str) -> OwnedEventId {
    EventId::parse(format!("$e{n}:{server}")).unwrap()
}

/// A small, fixed room: created by `@creator:hs1`, with `@member:hs1` already joined and
/// `@outsider:hs2` not in the room. Callers add or override state before building `FlatState`s.
struct Room {
    version: RoomVersionId,
    rules: RoomVersionRules,
    room_id: OwnedRoomId,
    creator: OwnedUserId,
    member: OwnedUserId,
    outsider: OwnedUserId,
    /// `(event_type, state_key) -> TestEvent`, in creation order.
    events: HashMap<(String, String), TestEvent>,
    order: Vec<(String, String)>,
}

impl Room {
    fn build(version: RoomVersionId, join_rule: &str) -> Self {
        let rules = room_version::rules_for(&version).expect("known room version");
        let room_id: OwnedRoomId = RoomId::parse("!room:hs1").unwrap();
        let creator: OwnedUserId = UserId::parse("@creator:hs1").unwrap();
        let member: OwnedUserId = UserId::parse("@member:hs1").unwrap();
        let outsider: OwnedUserId = UserId::parse("@outsider:hs2").unwrap();

        let mut room = Self {
            version: version.clone(),
            rules,
            room_id: room_id.clone(),
            creator: creator.clone(),
            member: member.clone(),
            outsider,
            events: HashMap::new(),
            order: Vec::new(),
        };

        let create_content = if room.rules.use_room_create_sender {
            json!({ "room_version": version.as_str() })
        } else {
            json!({ "room_version": version.as_str(), "creator": creator.as_str() })
        };
        room.push_state(
            0,
            "m.room.create",
            "",
            &creator,
            create_content,
            vec![],
            vec![],
        );
        let create_id = room.id_of("m.room.create", "");

        room.push_state(
            1,
            "m.room.member",
            creator.as_str(),
            &creator,
            json!({ "membership": "join" }),
            vec![create_id.clone()],
            vec![create_id.clone()],
        );
        let join_id = room.id_of("m.room.member", creator.as_str());

        room.push_state(
            2,
            "m.room.power_levels",
            "",
            &creator,
            json!({
                "users": { creator.as_str(): 100 },
                "users_default": 0,
                "events_default": 0,
                "state_default": 50,
                "ban": 50, "kick": 50, "redact": 50, "invite": 0,
            }),
            vec![join_id.clone()],
            vec![create_id.clone(), join_id.clone()],
        );
        let pl_id = room.id_of("m.room.power_levels", "");

        room.push_state(
            3,
            "m.room.join_rules",
            "",
            &creator,
            json!({ "join_rule": join_rule }),
            vec![pl_id.clone()],
            vec![create_id.clone(), join_id.clone(), pl_id.clone()],
        );
        let jr_id = room.id_of("m.room.join_rules", "");

        room.push_state(
            4,
            "m.room.member",
            member.as_str(),
            &creator,
            json!({ "membership": "join" }),
            vec![jr_id.clone()],
            vec![create_id.clone(), pl_id.clone(), jr_id.clone()],
        );

        room
    }

    fn id_of(&self, event_type: &str, state_key: &str) -> OwnedEventId {
        self.events[&(event_type.to_owned(), state_key.to_owned())]
            .id
            .clone()
    }

    #[allow(clippy::too_many_arguments)]
    fn push_state(
        &mut self,
        n: u32,
        event_type: &str,
        state_key: &str,
        sender: &UserId,
        content: serde_json::Value,
        prev_events: Vec<OwnedEventId>,
        auth_events: Vec<OwnedEventId>,
    ) {
        let id = event_id(n, "hs1");
        let raw = serde_json::value::to_raw_value(&content).unwrap();
        let event = TestEvent {
            id: id.clone(),
            room_id: self.room_id.clone(),
            sender: sender.to_owned(),
            event_type: TimelineEventType::from(event_type),
            content: raw,
            state_key: Some(state_key.to_owned()),
            prev_events,
            auth_events,
            redacts: None,
            rejected: false,
        };
        let key = (event_type.to_owned(), state_key.to_owned());
        self.events.insert(key.clone(), event);
        self.order.push(key);
    }

    /// The room's current state as a `FlatState` for `hs_state::auth`.
    fn flat_state(&self) -> FlatState {
        let mut state = FlatState::new();
        for key in &self.order {
            let event = &self.events[key];
            let content = raw_to_canonical(&event.content, self.rules.strict_canonical_json);
            state.insert(key.0.clone(), key.1.clone(), event.sender.clone(), content);
        }
        state
    }

    /// A `fetch_state` closure for `ruma_state_res`.
    fn ruma_fetch_state(&self) -> impl Fn(&StateEventType, &str) -> Option<TestEvent> + '_ {
        move |event_type, state_key| {
            self.events
                .get(&(event_type.to_string(), state_key.to_owned()))
                .cloned()
        }
    }

    /// A `fetch_event` closure for `ruma_state_res` (looks events up by ID across all state
    /// events currently in the room; enough for the auth-events-selection check in this fixture).
    fn ruma_fetch_event(&self) -> impl Fn(&EventId) -> Option<TestEvent> + '_ {
        move |id| {
            self.events
                .values()
                .find(|e| AsRef::<EventId>::as_ref(&e.id) == id)
                .cloned()
        }
    }

    /// The `AuthEventRef`s for `hs_state::auth::check_auth_events_selection`, matching the
    /// `auth_events` ids this fixture would naturally pick (every state event currently in the
    /// room except `m.room.create` when the room version derives the room ID from it).
    fn auth_event_refs(&self) -> Vec<AuthEventRef<'_>> {
        self.order
            .iter()
            .filter(|key| key.0 != "m.room.create" || !self.rules.room_create_event_id_as_room_id)
            .map(|key| AuthEventRef {
                event_type: &key.0,
                state_key: &key.1,
                rejected: false,
            })
            .collect()
    }
}

fn raw_to_canonical(raw: &RawJsonValue, strict: bool) -> CanonicalJsonObject {
    let value: serde_json::Value = serde_json::from_str(raw.get()).unwrap();
    to_canonical_object(&value, strict).unwrap()
}

/// One synthetic candidate event to check auth for.
#[derive(Debug, Clone)]
enum Candidate {
    Message {
        sender: Who,
    },
    Join {
        target: Who,
    },
    Invite {
        sender: Who,
        target: Who,
    },
    Leave {
        sender: Who,
        target: Who,
    },
    Ban {
        sender: Who,
        target: Who,
    },
    PowerLevelsChange {
        sender: Who,
        new_ban: i64,
        new_users_default: i64,
    },
}

#[derive(Debug, Clone, Copy)]
enum Who {
    Creator,
    Member,
    Outsider,
}

fn who_strategy() -> impl Strategy<Value = Who> {
    prop_oneof![Just(Who::Creator), Just(Who::Member), Just(Who::Outsider)]
}

fn candidate_strategy() -> impl Strategy<Value = Candidate> {
    prop_oneof![
        who_strategy().prop_map(|sender| Candidate::Message { sender }),
        who_strategy().prop_map(|target| Candidate::Join { target }),
        (who_strategy(), who_strategy())
            .prop_map(|(sender, target)| Candidate::Invite { sender, target }),
        (who_strategy(), who_strategy())
            .prop_map(|(sender, target)| Candidate::Leave { sender, target }),
        (who_strategy(), who_strategy())
            .prop_map(|(sender, target)| Candidate::Ban { sender, target }),
        (who_strategy(), 0i64..100, 0i64..100).prop_map(|(sender, new_ban, new_users_default)| {
            Candidate::PowerLevelsChange {
                sender,
                new_ban,
                new_users_default,
            }
        }),
    ]
}

struct Built {
    event_type: &'static str,
    sender: OwnedUserId,
    state_key: Option<String>,
    content: serde_json::Value,
}

fn build(candidate: &Candidate, room: &Room) -> Built {
    let who = |w: Who| match w {
        Who::Creator => room.creator.clone(),
        Who::Member => room.member.clone(),
        Who::Outsider => room.outsider.clone(),
    };
    match candidate {
        Candidate::Message { sender } => Built {
            event_type: "m.room.message",
            sender: who(*sender),
            state_key: None,
            content: json!({"body": "hi", "msgtype": "m.text"}),
        },
        Candidate::Join { target } => {
            let t = who(*target);
            Built {
                event_type: "m.room.member",
                sender: t.clone(),
                state_key: Some(t.to_string()),
                content: json!({"membership": "join"}),
            }
        }
        Candidate::Invite { sender, target } => Built {
            event_type: "m.room.member",
            sender: who(*sender),
            state_key: Some(who(*target).to_string()),
            content: json!({"membership": "invite"}),
        },
        Candidate::Leave { sender, target } => Built {
            event_type: "m.room.member",
            sender: who(*sender),
            state_key: Some(who(*target).to_string()),
            content: json!({"membership": "leave"}),
        },
        Candidate::Ban { sender, target } => Built {
            event_type: "m.room.member",
            sender: who(*sender),
            state_key: Some(who(*target).to_string()),
            content: json!({"membership": "ban"}),
        },
        Candidate::PowerLevelsChange {
            sender,
            new_ban,
            new_users_default,
        } => Built {
            event_type: "m.room.power_levels",
            sender: who(*sender),
            state_key: Some(String::new()),
            content: json!({
                "users": { room.creator.as_str(): 100 },
                "users_default": new_users_default,
                "events_default": 0,
                "state_default": 50,
                "ban": new_ban, "kick": 50, "redact": 50, "invite": 0,
            }),
        },
    }
}

/// Runs both checkers on `built` against `room`'s fixed state and returns whether each allowed
/// it.
fn check_both(room: &Room, built: &Built) -> (bool, bool) {
    let content = to_canonical_object(&built.content, room.rules.strict_canonical_json).unwrap();
    let prev = room.id_of("m.room.member", room.member.as_str());
    let incoming = IncomingEvent {
        event_type: built.event_type,
        sender: &built.sender,
        room_id: Some(&room.room_id),
        state_key: built.state_key.as_deref(),
        content: &content,
        prev_event_count: 1,
        only_prev_event_is_room_create: false,
        event_id: None,
        redacts: None,
    };
    let state = room.flat_state();
    let ours = auth::check_event_auth(&room.rules, &incoming, &state).is_ok()
        && auth::check_auth_events_selection(
            &room.rules,
            &incoming,
            &room.auth_event_refs(),
            || Ok(true),
        )
        .is_ok();

    let raw = serde_json::value::to_raw_value(&built.content).unwrap();
    let auth_events: Vec<OwnedEventId> = room
        .auth_event_refs()
        .iter()
        .map(|r| room.id_of(r.event_type, r.state_key))
        .collect();
    let ruma_incoming = TestEvent {
        id: event_id(999, "hs2"),
        room_id: room.room_id.clone(),
        sender: built.sender.clone(),
        event_type: TimelineEventType::from(built.event_type),
        content: raw,
        state_key: built.state_key.clone(),
        prev_events: vec![prev],
        auth_events,
        redacts: None,
        rejected: false,
    };

    let ruma_rules = room.version.rules().expect("known room version");
    let auth_rules = &ruma_rules.authorization;
    let theirs = ruma::state_res::check_state_independent_auth_rules(
        auth_rules,
        ruma_incoming.clone(),
        room.ruma_fetch_event(),
    )
    .and_then(|()| {
        ruma::state_res::check_state_dependent_auth_rules(
            auth_rules,
            ruma_incoming,
            room.ruma_fetch_state(),
        )
    })
    .is_ok();

    (ours, theirs)
}

fn versions_and_join_rules() -> Vec<(RoomVersionId, &'static str)> {
    let versions = [
        RoomVersionId::V1,
        RoomVersionId::V6,
        RoomVersionId::V8,
        RoomVersionId::V10,
        RoomVersionId::V11,
        RoomVersionId::V12,
    ];
    let mut out = Vec::new();
    for v in versions {
        out.push((v.clone(), "public"));
        out.push((v, "invite"));
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn agrees_with_ruma_state_res_on_random_events(candidate in candidate_strategy(), room_idx in 0usize..12) {
        let (version, join_rule) = versions_and_join_rules()[room_idx].clone();
        let room = Room::build(version, join_rule);
        let built = build(&candidate, &room);
        let (ours, theirs) = check_both(&room, &built);
        prop_assert_eq!(ours, theirs, "mismatch for {:?} in room version {} with join_rule {}", candidate, room.version.as_str(), join_rule);
    }
}
