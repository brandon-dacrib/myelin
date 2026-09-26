//! `RoomActor::accept_backfilled_events` and what `RoomActor::paginate` says at the edge of what
//! is held: the history of a room this server's own user joined elsewhere (RFC 0015 built it
//! from the resident's snapshot with the join as its only timeline event) is fetched after the
//! fact and placed *below* the join, in the resident's own order, with the state at each event
//! computed by walking back from the join (`crate::backfill`'s module docs, and the method's,
//! say how and how exactly).
//!
//! As in `tests/remote_join.rs`, two backends stand in for two homeservers and nothing checks a
//! signature: the resident (`a.example`) answers "what came before the join" with a page of its
//! own timeline, which is exactly what its `/backfill` endpoint serves, and the joining side
//! (`b.example`) files it.

use std::collections::BTreeSet;

use hs_kv::memory::MemoryBackend;
use hs_model::Event;
use hs_room::actor::{CreateRoomRequest, RemoteEventOutcome, RoomActor, StateAtEvent};
use hs_room::identity::HomeserverIdentity;
use hs_room::membership::Action;
use hs_room::persist::Tables;
use hs_room::timeline::{Direction, PaginationToken};
use ruma::{OwnedEventId, OwnedRoomId, RoomVersionId, user_id};
use serde_json::json;

/// The room as `a.example` hosts it: created by alice, three messages and two name changes
/// before bob joins, and the snapshot its `send_join` hands bob's server.
struct Resident {
    actor: RoomActor<MemoryBackend>,
    room_id: OwnedRoomId,
    join: Event,
    state: Vec<Event>,
    auth_chain: Vec<Event>,
    /// Alice's messages, in the order she sent them.
    messages: Vec<Event>,
}

fn resident() -> Resident {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    let identity = HomeserverIdentity::for_tests("a.example");
    let alice = user_id!("@alice:a.example").to_owned();
    let bob = user_id!("@bob:b.example").to_owned();
    let mut actor = RoomActor::create_room(
        backend,
        tables,
        identity,
        alice.clone(),
        CreateRoomRequest {
            preset: Some("public_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            ..Default::default()
        },
        1,
    )
    .expect("create the resident room");
    let mut messages = Vec::new();
    let mut ts = 2;
    for (i, name) in [None, Some("first name"), None, Some("second name"), None]
        .into_iter()
        .enumerate()
    {
        match name {
            Some(name) => {
                actor
                    .send_event(
                        alice.clone(),
                        "m.room.name".to_owned(),
                        Some(String::new()),
                        json!({"name": name}),
                        None,
                        ts,
                    )
                    .expect("alice names the room");
            }
            None => {
                let message = actor
                    .send_event(
                        alice.clone(),
                        "m.room.message".to_owned(),
                        None,
                        json!({"msgtype": "m.text", "body": format!("message {}", i + 1)}),
                        None,
                        ts,
                    )
                    .expect("alice sends a message");
                messages.push(message);
            }
        }
        ts += 1;
    }
    let join = actor
        .membership_action(bob.clone(), Action::Join, bob, json!({}), ts)
        .expect("bob joins the public room");
    let last = messages.last().expect("three messages");
    let StateAtEvent { state, auth_chain } = actor
        .state_at_event(last.event_id())
        .expect("state lookup")
        .expect("the message is known");
    let room_id = actor.room_id().to_owned();
    Resident {
        actor,
        room_id,
        join,
        state,
        auth_chain,
        messages,
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

/// What the resident's `/backfill?v=<from>&limit=<limit>` answers: its timeline from `from`
/// (included) backwards, newest first -- `hs_cli::federation::RegistryRoomSource::backfill`'s
/// exact page.
fn resident_backfill(resident: &Resident, from: &Event, limit: usize) -> Vec<Event> {
    let pos = resident
        .actor
        .timeline_position(from.event_id())
        .expect("the resident holds the event in its timeline");
    let (page, _) = resident.actor.paginate(
        Some(PaginationToken::new(pos + 1, Direction::Backward)),
        Direction::Backward,
        limit,
    );
    page.into_iter().cloned().collect()
}

fn ids<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<OwnedEventId> {
    events
        .into_iter()
        .map(|e| e.event_id().to_owned())
        .collect()
}

fn whole_timeline_newest_first(actor: &RoomActor<MemoryBackend>) -> Vec<OwnedEventId> {
    let (page, next) = actor.paginate(None, Direction::Backward, 1000);
    assert!(next.is_none(), "1000 is more than the whole room");
    ids(page)
}

fn state_at(actor: &RoomActor<MemoryBackend>, event: &Event) -> BTreeSet<OwnedEventId> {
    actor
        .state_at_event(event.event_id())
        .expect("state lookup")
        .expect("the event is known")
        .state
        .iter()
        .map(|e| e.event_id().to_owned())
        .collect()
}

#[test]
fn a_page_at_the_held_edge_names_the_boundary_while_history_continues_before_it() {
    let resident = resident();
    let joiner = joiner(&resident);
    let bob = user_id!("@bob:b.example");

    // Only the join is held, and the room did not begin with it.
    assert!(joiner.actor.history_before_oldest());
    let anchor = joiner
        .actor
        .backfill_anchor()
        .expect("there is history to fetch");
    assert_eq!(anchor.event_id, resident.join.event_id());
    assert_eq!(
        anchor.servers,
        vec!["a.example".to_owned()],
        "the room's server, then the other members' servers, never this one"
    );

    // A backward page reaches the join and says so -- with a token, because the history goes
    // on before it, where before this the page was "the start of the room".
    let page = joiner.actor.paginate_page(None, Direction::Backward, 10);
    assert_eq!(ids(page.events), ids([&resident.join]));
    assert!(page.reached_edge);
    assert_eq!(
        page.next,
        Some(PaginationToken::new(1, Direction::Backward))
    );
    // And from that token, an empty page at the edge hands the same boundary back rather than
    // nothing: after backfill, that is where the next page continues from.
    let again = joiner
        .actor
        .paginate_page(page.next, Direction::Backward, 10);
    assert!(again.events.is_empty());
    assert!(again.reached_edge);
    assert_eq!(
        again.next,
        Some(PaginationToken::new(1, Direction::Backward))
    );

    // A room created here begins with its create event, and a page that reaches it has no
    // token: exactly as before.
    let resident_page = resident
        .actor
        .paginate_page(None, Direction::Backward, 1000);
    assert!(!resident.actor.history_before_oldest());
    assert!(resident.actor.backfill_anchor().is_none());
    assert!(resident_page.reached_edge);
    assert!(resident_page.next.is_none());

    // Nothing about visibility changed: bob may read his own join.
    assert!(
        joiner
            .actor
            .event_visible_to(&resident.join, bob)
            .expect("visibility")
    );
}

#[test]
fn history_before_the_join_is_placed_below_it_in_the_residents_order() {
    let resident = resident();
    let mut joiner = joiner(&resident);
    let bob = user_id!("@bob:b.example");
    let mut published = joiner.actor.subscribe();

    let batch = resident_backfill(&resident, &resident.join, 100);
    assert_eq!(
        batch.len(),
        12,
        "the resident's whole timeline: six creation events, five of alice's, the join"
    );
    let added = joiner
        .actor
        .accept_backfilled_events(batch.clone())
        .expect("the batch is stored");
    // Everything but the join, which was already in the timeline: five new events and the six
    // creation events the snapshot already held as outliers, now placed.
    assert_eq!(added, 11);

    // --- the timeline is now the resident's, in the resident's order, and it begins at the
    // create event ---
    assert_eq!(
        whole_timeline_newest_first(&joiner.actor),
        whole_timeline_newest_first(&resident.actor)
    );
    assert!(!joiner.actor.history_before_oldest());
    assert!(joiner.actor.backfill_anchor().is_none());
    let page = joiner.actor.paginate_page(None, Direction::Backward, 5);
    assert_eq!(page.events.len(), 5);
    assert!(!page.reached_edge);
    assert!(page.next.is_some());

    // --- below the join: the newest of the batch at -1, the create event furthest down ---
    let last_message = resident.messages.last().expect("three messages");
    assert_eq!(
        joiner.actor.timeline_position(resident.join.event_id()),
        Some(1)
    );
    assert_eq!(
        joiner.actor.timeline_position(last_message.event_id()),
        Some(-1)
    );
    let create = resident
        .state
        .iter()
        .find(|e| e.header().event_type == "m.room.create")
        .expect("the snapshot has the create event");
    assert_eq!(joiner.actor.timeline_position(create.event_id()), Some(-11));

    // --- a placed outlier is still an outlier ---
    let held_create = joiner
        .actor
        .event_by_id(create.event_id())
        .expect("the create event is held");
    assert!(held_create.header().flags.is_outlier());

    // --- the state at each of alice's events is exactly what the resident has for it: the
    // walk back from the join reverted both name changes to what preceded them ---
    for message in &resident.messages {
        assert_eq!(
            state_at(&joiner.actor, message),
            state_at(&resident.actor, message),
            "state at {}",
            message.event_id()
        );
    }
    let names = |actor: &RoomActor<MemoryBackend>, message: &Event| -> Option<String> {
        actor
            .state_at_event(message.event_id())
            .expect("state lookup")
            .expect("known")
            .state
            .iter()
            .find(|e| e.header().event_type == "m.room.name")
            .map(|e| {
                e.json()
                    .get("content")
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_object)
                    .and_then(|c| c.get("name"))
                    .and_then(hs_model::canonical::CanonicalJsonValue::as_str)
                    .unwrap_or("")
                    .to_owned()
            })
    };
    assert_eq!(names(&joiner.actor, &resident.messages[0]), None);
    assert_eq!(
        names(&joiner.actor, &resident.messages[1]),
        Some("first name".to_owned())
    );
    assert_eq!(
        names(&joiner.actor, &resident.messages[2]),
        Some("second name".to_owned())
    );

    // --- bob may read it all: `shared` history, and he joined later ---
    for event in &batch {
        assert!(
            joiner
                .actor
                .event_visible_to(event, bob)
                .expect("visibility"),
            "{} should be visible to bob",
            event.event_id()
        );
    }

    // --- history is not news: nothing was published, and a follower with a cursor sees
    // nothing new ---
    assert!(
        matches!(
            published.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "a backfilled event must not be published as an update"
    );
    assert_eq!(
        ids(joiner
            .actor
            .events_after(0, 100)
            .into_iter()
            .map(|(_, e)| e)),
        ids([&resident.join])
    );

    // --- the same batch again adds nothing ---
    assert_eq!(
        joiner
            .actor
            .accept_backfilled_events(batch)
            .expect("stored again"),
        0
    );

    // --- and a reload from the store comes back the same ---
    let reloaded = RoomActor::load(
        joiner.backend.clone(),
        joiner.tables.clone(),
        joiner.identity.clone(),
        &resident.room_id,
    )
    .expect("load")
    .expect("the room exists");
    assert_eq!(
        whole_timeline_newest_first(&reloaded),
        whole_timeline_newest_first(&resident.actor)
    );
    assert!(!reloaded.history_before_oldest());
    for message in &resident.messages {
        assert_eq!(
            state_at(&reloaded, message),
            state_at(&resident.actor, message),
            "state at {} after reload",
            message.event_id()
        );
    }
    assert!(
        reloaded
            .event_by_id(create.event_id())
            .expect("held")
            .header()
            .flags
            .is_outlier()
    );
    assert_eq!(reloaded.timeline_position(create.event_id()), Some(-11));
}

#[test]
fn a_second_batch_continues_from_where_the_first_stopped() {
    let resident = resident();
    let mut joiner = joiner(&resident);

    // The resident answers four at a time: the join and the three newest before it.
    let first = resident_backfill(&resident, &resident.join, 4);
    assert_eq!(first.len(), 4);
    assert_eq!(
        joiner
            .actor
            .accept_backfilled_events(first)
            .expect("first batch"),
        3
    );
    assert!(joiner.actor.history_before_oldest());
    let anchor = joiner.actor.backfill_anchor().expect("more to fetch");
    let oldest_held = joiner
        .actor
        .event_by_id(&anchor.event_id)
        .expect("held")
        .clone();
    assert_eq!(joiner.actor.timeline_position(&anchor.event_id), Some(-3));
    assert_eq!(
        oldest_held.event_id(),
        resident.messages[1].event_id(),
        "the oldest held is now alice's second message"
    );

    // The next batch walks back from that event; it comes back too and is skipped.
    let second = resident_backfill(&resident, &oldest_held, 100);
    assert_eq!(
        joiner
            .actor
            .accept_backfilled_events(second)
            .expect("second batch"),
        8
    );
    assert!(!joiner.actor.history_before_oldest());
    assert_eq!(
        whole_timeline_newest_first(&joiner.actor),
        whole_timeline_newest_first(&resident.actor)
    );
    // The state at the first message was computed from the second batch's own walk, which
    // started from the state the first batch wrote for its oldest event.
    assert_eq!(
        state_at(&joiner.actor, &resident.messages[0]),
        state_at(&resident.actor, &resident.messages[0])
    );
}

#[test]
fn an_event_newer_than_the_oldest_held_is_not_history() {
    let mut resident = resident();
    let mut joiner = joiner(&resident);
    let alice = user_id!("@alice:a.example").to_owned();

    // Alice speaks after bob joined: that event is on its way over `/send`, not history.
    let after = resident
        .actor
        .send_event(
            alice,
            "m.room.message".to_owned(),
            None,
            json!({"msgtype": "m.text", "body": "after bob"}),
            None,
            50,
        )
        .expect("alice posts after the join");
    let mut batch = resident_backfill(&resident, &resident.join, 100);
    batch.push(after.clone());
    joiner
        .actor
        .accept_backfilled_events(batch)
        .expect("the batch is stored");
    assert!(
        joiner.actor.event_by_id(after.event_id()).is_none(),
        "an event newer than the anchor must not be filed as history"
    );

    // When it does arrive live, it goes where live events go.
    let outcome = joiner
        .actor
        .accept_remote_event(after.clone())
        .expect("accepted live");
    assert!(matches!(outcome, RemoteEventOutcome::Stored(_)));
    assert_eq!(joiner.actor.timeline_position(after.event_id()), Some(2));
}
