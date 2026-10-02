//! The state at backfilled history, asked for rather than walked, and every backfilled event
//! authorized at its position (`hs_room::actor::RoomActor::accept_history`): the resident's
//! answer to `/state_ids` at a batch's oldest event (here, `RoomActor::state_before_event` on
//! the resident's own actor, which is what `hs-cli`'s `/state_ids` serves) is where the state at
//! every event of the batch is derived from, and an event the state before it does not allow is
//! not placed.
//!
//! As in `tests/backfill.rs`, two backends stand in for two homeservers and nothing checks a
//! signature.

use std::collections::BTreeSet;

use hs_kv::memory::MemoryBackend;
use hs_model::Event;
use hs_model::canonical::CanonicalJsonValue;
use hs_room::actor::{CreateRoomRequest, RoomActor, StateAtEvent};
use hs_room::backfill::{FetchedState, HistoryKind, HistoryPlan, StateSource};
use hs_room::identity::HomeserverIdentity;
use hs_room::membership::Action;
use hs_room::persist::Tables;
use hs_room::timeline::{Direction, PaginationToken};
use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomVersionId, user_id};
use serde_json::json;

fn alice() -> OwnedUserId {
    user_id!("@alice:a.example").to_owned()
}

fn mallory() -> OwnedUserId {
    user_id!("@mallory:a.example").to_owned()
}

/// `a.example`'s room: alice sets a topic, says three things; mallory joins, speaks and
/// leaves; alice changes the topic and says three more things; then bob (`b.example`) joins.
struct Resident {
    actor: RoomActor<MemoryBackend>,
    room_id: OwnedRoomId,
    join: Event,
    state: Vec<Event>,
    auth_chain: Vec<Event>,
    mallory_join: Event,
    mallory_leave: Event,
    first_topic: Event,
    second_topic: Event,
    /// Alice's three messages after the second topic.
    after: Vec<Event>,
}

fn say(actor: &mut RoomActor<MemoryBackend>, who: OwnedUserId, body: &str, ts: i64) -> Event {
    actor
        .send_event(
            who,
            "m.room.message".to_owned(),
            None,
            json!({"msgtype": "m.text", "body": body}),
            None,
            ts,
        )
        .expect("a message is sent")
}

fn topic(actor: &mut RoomActor<MemoryBackend>, text: &str, ts: i64) -> Event {
    actor
        .send_event(
            alice(),
            "m.room.topic".to_owned(),
            Some(String::new()),
            json!({"topic": text}),
            None,
            ts,
        )
        .expect("alice sets the topic")
}

fn resident() -> Resident {
    let backend = MemoryBackend::new();
    let mut actor = RoomActor::create_room(
        backend.clone(),
        Tables::open(&backend).expect("open tables"),
        HomeserverIdentity::for_tests("a.example"),
        alice(),
        CreateRoomRequest {
            preset: Some("public_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            ..Default::default()
        },
        1,
    )
    .expect("create the resident room");
    let mut ts = 10;
    let mut tick = || {
        ts += 1;
        ts
    };
    let first_topic = topic(&mut actor, "first topic", tick());
    for i in 1..=3 {
        say(&mut actor, alice(), &format!("before {i}"), tick());
    }
    let mallory_join = actor
        .membership_action(mallory(), Action::Join, mallory(), json!({}), tick())
        .expect("mallory joins");
    say(&mut actor, mallory(), "mallory was here", tick());
    let mallory_leave = actor
        .membership_action(mallory(), Action::Leave, mallory(), json!({}), tick())
        .expect("mallory leaves");
    let second_topic = topic(&mut actor, "second topic", tick());
    let mut after = Vec::new();
    for i in 1..=3 {
        after.push(say(&mut actor, alice(), &format!("after {i}"), tick()));
    }
    let bob = user_id!("@bob:b.example").to_owned();
    let join = actor
        .membership_action(bob.clone(), Action::Join, bob, json!({}), tick())
        .expect("bob joins");
    let StateAtEvent { state, auth_chain } = actor
        .state_at_event(after.last().expect("three").event_id())
        .expect("state lookup")
        .expect("known");
    let room_id = actor.room_id().to_owned();
    Resident {
        actor,
        room_id,
        join,
        state,
        auth_chain,
        mallory_join,
        mallory_leave,
        first_topic,
        second_topic,
        after,
    }
}

struct Joiner {
    actor: RoomActor<MemoryBackend>,
    backend: MemoryBackend,
    tables: Tables<MemoryBackend>,
    identity: HomeserverIdentity,
}

fn joiner(resident: &Resident) -> Joiner {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    let identity = HomeserverIdentity::for_tests("b.example");
    let actor = RoomActor::create_from_remote_join(
        backend.clone(),
        tables.clone(),
        identity.clone(),
        &resident.room_id,
        RoomVersionId::V11,
        resident.state.clone(),
        resident.auth_chain.clone(),
        resident.join.clone(),
    )
    .expect("bootstrap from the join response");
    Joiner {
        actor,
        backend,
        tables,
        identity,
    }
}

/// The resident's `/backfill` from `from`, `limit` events, newest first.
fn resident_backfill(resident: &Resident, from: &Event, limit: usize) -> Vec<Event> {
    let pos = resident
        .actor
        .timeline_position(from.event_id())
        .expect("in the resident's timeline");
    let (page, _) = resident.actor.paginate(
        Some(PaginationToken::new(pos + 1, Direction::Backward)),
        Direction::Backward,
        limit,
    );
    page.into_iter().cloned().collect()
}

/// What `hs-cli`'s `FederationBackfill` does with a plan: `/state_ids` at the oldest event,
/// then every event named there, in its auth chain or in the batch's `auth_events` that the
/// joiner does not hold, as `/event` would hand it over.
fn fetch_state(resident: &Resident, joiner: &Joiner, plan: &HistoryPlan) -> FetchedState {
    let StateAtEvent { state, auth_chain } = resident
        .actor
        .state_before_event(&plan.oldest)
        .expect("state lookup")
        .expect("the resident knows the event");
    let state_ids: Vec<OwnedEventId> = state.iter().map(|e| e.event_id().to_owned()).collect();
    let mut wanted = state_ids.clone();
    wanted.extend(auth_chain.iter().map(|e| e.event_id().to_owned()));
    wanted.extend(plan.missing_auth.iter().cloned());
    let events = joiner
        .actor
        .events_not_held(&wanted)
        .iter()
        .filter_map(|id| resident.actor.event_by_id(id).cloned())
        .collect();
    FetchedState {
        at: plan.oldest.clone(),
        state_ids,
        events,
    }
}

fn state_set(state: &StateAtEvent) -> BTreeSet<OwnedEventId> {
    state
        .state
        .iter()
        .map(|e| e.event_id().to_owned())
        .collect()
}

fn state_ids(actor: &RoomActor<MemoryBackend>, event: &EventId) -> BTreeSet<OwnedEventId> {
    actor
        .state_at_event(event)
        .expect("state lookup")
        .expect("the event is known")
        .state
        .iter()
        .map(|e| e.event_id().to_owned())
        .collect()
}

fn topic_at(actor: &RoomActor<MemoryBackend>, event: &EventId) -> Option<String> {
    actor
        .state_at_event(event)
        .expect("state lookup")
        .expect("the event is known")
        .state
        .iter()
        .find(|e| e.header().event_type == "m.room.topic")
        .and_then(|e| {
            e.json()
                .get("content")
                .and_then(CanonicalJsonValue::as_object)
                .and_then(|c| c.get("topic"))
                .and_then(CanonicalJsonValue::as_str)
                .map(str::to_owned)
        })
}

/// A copy of `template` (one of the resident's messages) re-sent as `sender`, citing
/// `auth_events` and following `prev`: an event no honest server would have built, signed by
/// nobody (nothing here checks), with an event ID of its own.
fn forged(template: &Event, sender: &str, auth_events: &[&Event], prev: &Event) -> Event {
    let mut json: serde_json::Value =
        serde_json::from_slice(template.canonical_bytes()).expect("json");
    let object = json.as_object_mut().expect("an object");
    object.insert("sender".to_owned(), json!(sender));
    object.insert(
        "auth_events".to_owned(),
        json!(
            auth_events
                .iter()
                .map(|e| e.event_id().to_string())
                .collect::<Vec<_>>()
        ),
    );
    object.insert(
        "prev_events".to_owned(),
        json!([prev.event_id().to_string()]),
    );
    object.insert("depth".to_owned(), json!(prev.header().depth + 1));
    object.insert(
        "content".to_owned(),
        json!({"msgtype": "m.text", "body": format!("forged by {sender}")}),
    );
    Event::parse(&json, RoomVersionId::V11).expect("a well-formed event")
}

fn state_event<'a>(resident: &'a Resident, event_type: &str) -> &'a Event {
    resident
        .actor
        .state_event(event_type, "")
        .expect("state lookup")
        .expect("the room has it")
}

#[test]
fn a_key_set_before_the_batch_is_there_at_every_event_with_the_fetched_state() {
    let resident = resident();
    let mut joiner = joiner(&resident);

    // Six back from the join: alice's three, the second topic, mallory's leave -- and not the
    // first topic, which is older than the batch and no longer the room's state, so the
    // joiner has never held it.
    let batch = resident_backfill(&resident, &resident.join, 6);
    assert_eq!(
        batch.last().map(|e| e.event_id().to_owned()),
        Some(resident.mallory_leave.event_id().to_owned())
    );
    assert!(
        joiner
            .actor
            .event_by_id(resident.first_topic.event_id())
            .is_none()
    );

    let plan = joiner
        .actor
        .plan_history(HistoryKind::BeforeOldest, &batch)
        .expect("plan")
        .expect("something to place");
    assert_eq!(plan.oldest, resident.mallory_leave.event_id());
    let fetched = fetch_state(&resident, &joiner, &plan);
    assert!(
        fetched
            .events
            .iter()
            .any(|e| e.event_id() == resident.first_topic.event_id()),
        "the first topic is fetched: the joiner does not hold it"
    );
    let outcome = joiner
        .actor
        .accept_history(HistoryKind::BeforeOldest, batch.clone(), Some(fetched))
        .expect("the batch is placed");
    assert_eq!(outcome.state, StateSource::Fetched);
    assert_eq!(outcome.added, 5);
    assert_eq!(outcome.rejected, 0);
    assert!(outcome.state_events_stored >= 1);

    // The topic set before the batch is there at mallory's leave, and the state at every
    // placed event is exactly the resident's -- outliers placed (the leave, the second topic)
    // included.
    assert_eq!(
        topic_at(&joiner.actor, resident.mallory_leave.event_id()).as_deref(),
        Some("first topic")
    );
    for event in batch.iter().skip(1) {
        assert_eq!(
            state_ids(&joiner.actor, event.event_id()),
            state_ids(&resident.actor, event.event_id()),
            "state at {}",
            event.event_id()
        );
        assert!(topic_at(&joiner.actor, event.event_id()).is_some());
    }

    // The walk, for contrast: the first topic is not in the batch, so before the second topic
    // the key reads as unset -- the inexactness the fetch removes.
    let mut walker = self::joiner(&resident);
    walker
        .actor
        .accept_backfilled_events(batch)
        .expect("walked");
    assert_eq!(
        topic_at(&walker.actor, resident.mallory_leave.event_id()),
        None
    );

    // And it all survives a reload.
    let reloaded = RoomActor::load(
        joiner.backend.clone(),
        joiner.tables.clone(),
        joiner.identity.clone(),
        &resident.room_id,
    )
    .expect("load")
    .expect("the room exists");
    for event in resident
        .after
        .iter()
        .chain([&resident.second_topic, &resident.mallory_leave])
    {
        assert_eq!(
            state_ids(&reloaded, event.event_id()),
            state_ids(&resident.actor, event.event_id()),
            "state at {} after reload",
            event.event_id()
        );
    }
}

#[test]
fn a_backfilled_event_not_authorized_at_its_position_is_not_placed() {
    let resident = resident();
    let create = state_event(&resident, "m.room.create");
    let power_levels = state_event(&resident, "m.room.power_levels");
    let first_after = &resident.after[0];

    // Eve was never in the room, and her message cites no membership at all.
    let eve = forged(
        first_after,
        "@eve:a.example",
        &[create, power_levels],
        first_after,
    );
    // Mallory's cites her join -- which her own leave had already superseded by then: allowed
    // by its auth_events, refused by the state before it.
    let mallory_late = forged(
        first_after,
        "@mallory:a.example",
        &[create, power_levels, &resident.mallory_join],
        first_after,
    );
    let mut batch = resident_backfill(&resident, &resident.join, 100);
    batch.push(eve.clone());
    batch.push(mallory_late.clone());

    // With the state fetched -- from before the create event: nothing -- and derived forward
    // through the whole room: both refused.
    let mut joiner = joiner(&resident);
    let plan = joiner
        .actor
        .plan_history(HistoryKind::BeforeOldest, &batch)
        .expect("plan")
        .expect("something to place");
    assert_eq!(plan.oldest, create.event_id());
    let fetched = fetch_state(&resident, &joiner, &plan);
    assert!(fetched.state_ids.is_empty(), "nothing before the create");
    let outcome = joiner
        .actor
        .accept_history(HistoryKind::BeforeOldest, batch.clone(), Some(fetched))
        .expect("the batch is placed");
    assert_eq!(outcome.state, StateSource::Fetched);
    assert_eq!(outcome.rejected, 2, "{outcome:?}");
    for refused in [&eve, &mallory_late] {
        assert!(
            joiner.actor.event_by_id(refused.event_id()).is_none(),
            "{} must not be stored",
            refused.event_id()
        );
        assert_eq!(joiner.actor.timeline_position(refused.event_id()), None);
    }
    // Everything else is the resident's timeline, in its order, with its state.
    let (held, _) = joiner.actor.paginate(None, Direction::Backward, 1000);
    let (theirs, _) = resident.actor.paginate(None, Direction::Backward, 1000);
    let ids = |events: Vec<&Event>| -> Vec<OwnedEventId> {
        events.iter().map(|e| e.event_id().to_owned()).collect()
    };
    assert_eq!(ids(held), ids(theirs));
    for event in batch
        .iter()
        .filter(|e| e.event_id() != eve.event_id() && e.event_id() != mallory_late.event_id())
    {
        assert_eq!(
            state_ids(&joiner.actor, event.event_id()),
            state_ids(&resident.actor, event.event_id()),
            "state at {}",
            event.event_id()
        );
    }

    // Walked instead, the auth_events check still runs: eve is refused; mallory's late message,
    // whose auth_events allow it, could only be caught by a state the walk does not know.
    let mut walker = self::joiner(&resident);
    let outcome = walker
        .actor
        .accept_history(HistoryKind::BeforeOldest, batch, None)
        .expect("walked");
    assert_eq!(outcome.state, StateSource::Walked);
    assert_eq!(outcome.rejected, 1);
    assert!(walker.actor.event_by_id(eve.event_id()).is_none());
}

#[test]
fn a_placed_outlier_answers_the_state_computed_for_it_not_itself() {
    let resident = resident();
    let mut joiner = joiner(&resident);

    // The second topic and mallory's leave arrived with the join, as outliers.
    for outlier in [&resident.second_topic, &resident.mallory_leave] {
        assert!(
            joiner
                .actor
                .event_by_id(outlier.event_id())
                .expect("held")
                .header()
                .flags
                .is_outlier()
        );
    }

    // The whole history, walked: every outlier of the snapshot is placed.
    let batch = resident_backfill(&resident, &resident.join, 100);
    joiner
        .actor
        .accept_backfilled_events(batch.clone())
        .expect("placed");

    // The state after a placed outlier is the room's state there, not the outlier alone.
    for event in batch.iter().skip(1) {
        assert_eq!(
            state_ids(&joiner.actor, event.event_id()),
            state_ids(&resident.actor, event.event_id()),
            "state at {}",
            event.event_id()
        );
    }
    assert!(
        state_ids(&joiner.actor, resident.second_topic.event_id()).len() > 1,
        "more than the event itself"
    );

    // A later batch walked back from a placed outlier starts from that state, too: the state
    // before the oldest placed event is what the next walk begins with, and after a reload it
    // is all still there.
    let reloaded = RoomActor::load(
        joiner.backend.clone(),
        joiner.tables.clone(),
        joiner.identity.clone(),
        &resident.room_id,
    )
    .expect("load")
    .expect("the room exists");
    for outlier in [&resident.second_topic, &resident.mallory_leave] {
        assert_eq!(
            state_ids(&reloaded, outlier.event_id()),
            state_ids(&resident.actor, outlier.event_id()),
            "state at {} after reload",
            outlier.event_id()
        );
        assert_eq!(
            reloaded
                .state_before_event(outlier.event_id())
                .expect("lookup")
                .map(|s| s.state.len()),
            resident
                .actor
                .state_before_event(outlier.event_id())
                .expect("lookup")
                .map(|s| s.state.len()),
        );
    }
}

/// A room persisted before placement recorded the state at placed outliers (status 04
/// session 13): the rows are gone, and a load gives each placed outlier the state after the
/// event before it -- the same state here, where the history is linear -- counts them, and
/// writes the rows back when asked, so the next load repairs nothing.
#[test]
fn a_placed_outlier_without_a_state_row_is_repaired_on_load() {
    use hs_kv::{KvBackend, TransactConfig, transact};

    let resident = resident();
    let mut joiner = joiner(&resident);
    let batch = resident_backfill(&resident, &resident.join, 100);
    joiner
        .actor
        .accept_backfilled_events(batch.clone())
        .expect("placed");
    let placed = [&resident.second_topic, &resident.mallory_leave];

    // What the store looked like before: the placed outliers' state rows never written.
    let snapshot = joiner.backend.snapshot();
    let room_sn = joiner
        .tables
        .room_sn
        .lookup(&snapshot, resident.room_id.as_bytes())
        .expect("lookup")
        .expect("the room is interned");
    let sns: Vec<_> = placed
        .iter()
        .map(|event| {
            joiner
                .tables
                .event_sn
                .lookup(&snapshot, event.event_id().as_bytes())
                .expect("lookup")
                .expect("the event is interned")
        })
        .collect();
    transact(&joiner.backend, TransactConfig::default(), |txn| {
        for sn in &sns {
            joiner
                .tables
                .state_snapshots
                .delete(txn, &(room_sn, *sn))
                .map_err(|e| {
                    hs_kv::KvError::Aborted(Box::new(std::io::Error::other(e.to_string())))
                })?;
        }
        Ok(())
    })
    .expect("rows deleted");

    let mut reloaded = RoomActor::load(
        joiner.backend.clone(),
        joiner.tables.clone(),
        joiner.identity.clone(),
        &resident.room_id,
    )
    .expect("load")
    .expect("the room exists");
    assert_eq!(
        reloaded.repaired_outlier_states(),
        2,
        "both rows were missing"
    );
    for outlier in placed {
        assert_eq!(
            state_ids(&reloaded, outlier.event_id()),
            state_ids(&resident.actor, outlier.event_id()),
            "the repaired state at {} is the room's state there",
            outlier.event_id()
        );
        assert_eq!(
            reloaded
                .state_before_event(outlier.event_id())
                .expect("lookup")
                .map(|s| state_set(&s)),
            resident
                .actor
                .state_before_event(outlier.event_id())
                .expect("lookup")
                .map(|s| state_set(&s)),
            "the state before {} is the room's",
            outlier.event_id()
        );
    }

    assert_eq!(
        reloaded
            .persist_repaired_outlier_states()
            .expect("rows written"),
        2
    );
    assert_eq!(reloaded.repaired_outlier_states(), 0);
    let again = RoomActor::load(
        joiner.backend.clone(),
        joiner.tables.clone(),
        joiner.identity.clone(),
        &resident.room_id,
    )
    .expect("load")
    .expect("the room exists");
    assert_eq!(again.repaired_outlier_states(), 0, "the rows are back");
    for outlier in placed {
        assert_eq!(
            state_ids(&again, outlier.event_id()),
            state_ids(&resident.actor, outlier.event_id())
        );
    }
}

#[test]
fn a_fetched_state_without_a_create_event_is_not_used() {
    let resident = resident();
    let mut joiner = joiner(&resident);
    let batch = resident_backfill(&resident, &resident.join, 6);
    let plan = joiner
        .actor
        .plan_history(HistoryKind::BeforeOldest, &batch)
        .expect("plan")
        .expect("something to place");

    // An empty answer -- what this server's own `/state_ids` said for every room of version 3
    // or later until 2026-09-30 -- would have every event refused for want of a create event.
    // It is set aside and the batch walked, as if nothing had been fetched.
    let outcome = joiner
        .actor
        .accept_history(
            HistoryKind::BeforeOldest,
            batch,
            Some(FetchedState {
                at: plan.oldest,
                state_ids: Vec::new(),
                events: Vec::new(),
            }),
        )
        .expect("placed");
    assert_eq!(outcome.state, StateSource::Walked);
    assert_eq!(outcome.rejected, 0);
    assert_eq!(outcome.added, 5);
}

#[test]
fn state_before_an_event_does_not_include_it() {
    let resident = resident();
    let before = resident
        .actor
        .state_before_event(resident.second_topic.event_id())
        .expect("lookup")
        .expect("known");
    let topic = before
        .state
        .iter()
        .find(|e| e.header().event_type == "m.room.topic")
        .expect("a topic before the second");
    assert_eq!(topic.event_id(), resident.first_topic.event_id());
    let after = resident
        .actor
        .state_at_event(resident.second_topic.event_id())
        .expect("lookup")
        .expect("known");
    assert!(
        after
            .state
            .iter()
            .any(|e| e.event_id() == resident.second_topic.event_id())
    );
    // An outlier no backfill has placed has no known state.
    let joiner = joiner(&resident);
    assert!(
        joiner
            .actor
            .state_before_event(resident.second_topic.event_id())
            .expect("lookup")
            .is_none()
    );
}
