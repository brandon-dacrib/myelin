//! The gap a leave and a rejoin leave in the middle of a room's timeline
//! (`hs_room::actor::gaps`): bob, on `b.example`, joins alice's room on `a.example`, leaves,
//! and while nobody from `b.example` is in the room alice says more and renames it. His rejoin
//! goes through `a.example` and brings the room's current state; what was said meanwhile is
//! fetched when a backward page reaches it, and placed *between* his leave and his rejoin, so
//! that reading back from the rejoin walks through it in the resident's order and on into what
//! `b.example` held before.
//!
//! As in `tests/backfill.rs`, two backends stand in for two homeservers and nothing checks a
//! signature: the resident answers a `/backfill` with a page of its own timeline from the
//! events asked for, which is what its endpoint serves.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use hs_auth::requester::Requester;
use hs_kv::memory::MemoryBackend;
use hs_model::Event;
use hs_room::RoomError;
use hs_room::actor::{CreateRoomRequest, RemoteEventOutcome, RoomActor, StateAtEvent};
use hs_room::backfill::Backfill;
use hs_room::identity::HomeserverIdentity;
use hs_room::membership::Action;
use hs_room::persist::Tables;
use hs_room::registry::RoomRegistry;
use hs_room::routes::query::{MessagesQuery, get_messages};
use hs_room::state::{RoomRequester, RoomState};
use hs_room::timeline::{Direction, PaginationToken, TIMELINE_GAP_SPAN};
use ruma::{OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, user_id};
use serde_json::json;

fn alice() -> OwnedUserId {
    user_id!("@alice:a.example").to_owned()
}

fn bob() -> OwnedUserId {
    user_id!("@bob:b.example").to_owned()
}

/// Both servers, after bob joined, left and came back, with everything in between.
struct Rejoined {
    resident: RoomActor<MemoryBackend>,
    joiner: RoomActor<MemoryBackend>,
    backend: MemoryBackend,
    tables: Tables<MemoryBackend>,
    identity: HomeserverIdentity,
    room_id: OwnedRoomId,
    first_join: Event,
    leave: Event,
    /// What alice did while bob was out, in order: messages and one rename.
    while_out: Vec<Event>,
    rejoin: Event,
}

fn message(actor: &mut RoomActor<MemoryBackend>, body: &str, ts: i64) -> Event {
    actor
        .send_event(
            alice(),
            "m.room.message".to_owned(),
            None,
            json!({"msgtype": "m.text", "body": body}),
            None,
            ts,
        )
        .expect("alice sends a message")
}

/// The state `a.example`'s `send_join` would answer with for a join citing `before`.
fn join_snapshot(resident: &RoomActor<MemoryBackend>, before: &Event) -> StateAtEvent {
    resident
        .state_at_event(before.event_id())
        .expect("state lookup")
        .expect("the event is known")
}

/// `while_out_messages` messages from alice while bob is out, with a rename after the second.
fn rejoined(while_out_messages: usize) -> Rejoined {
    let resident_backend = MemoryBackend::new();
    let mut resident = RoomActor::create_room(
        resident_backend.clone(),
        Tables::open(&resident_backend).expect("open tables"),
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
    let room_id = resident.room_id().to_owned();
    let mut ts = 10;
    let mut last = message(&mut resident, "before bob", ts);

    // Bob joins through a.example.
    ts += 1;
    let first_join = resident
        .membership_action(bob(), Action::Join, bob(), json!({}), ts)
        .expect("bob joins");
    let StateAtEvent { state, auth_chain } = join_snapshot(&resident, &last);
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    let identity = HomeserverIdentity::for_tests("b.example");
    let mut joiner = RoomActor::create_from_remote_join(
        backend.clone(),
        tables.clone(),
        identity.clone(),
        &room_id,
        RoomVersionId::V11,
        state,
        auth_chain,
        first_join.clone(),
    )
    .expect("bootstrap from the join response");

    // He leaves from b.example, which sends the leave to a.example.
    ts += 1;
    let leave = joiner
        .membership_action(bob(), Action::Leave, bob(), json!({}), ts)
        .expect("bob leaves");
    assert!(matches!(
        resident.accept_remote_event(leave.clone()),
        Ok(RemoteEventOutcome::Stored(_))
    ));
    assert!(
        !joiner.local_user_joined(),
        "nobody from b.example is in the room now"
    );

    // While he is out, alice talks and renames the room; b.example hears none of it.
    let mut while_out = Vec::new();
    for i in 0..while_out_messages {
        ts += 1;
        last = message(&mut resident, &format!("while bob was out {}", i + 1), ts);
        while_out.push(last.clone());
        if i == 1 {
            ts += 1;
            last = resident
                .send_event(
                    alice(),
                    "m.room.name".to_owned(),
                    Some(String::new()),
                    json!({"name": "renamed while bob was out"}),
                    None,
                    ts,
                )
                .expect("alice renames the room");
            while_out.push(last.clone());
        }
    }

    // He comes back through a.example: the answer is applied to b.example's copy.
    ts += 1;
    let rejoin = resident
        .membership_action(bob(), Action::Join, bob(), json!({}), ts)
        .expect("bob rejoins");
    let StateAtEvent { state, auth_chain } = join_snapshot(&resident, &last);
    assert!(matches!(
        joiner.accept_remote_join_with_state(state, auth_chain, rejoin.clone()),
        Ok(RemoteEventOutcome::Stored(_))
    ));

    Rejoined {
        resident,
        joiner,
        backend,
        tables,
        identity,
        room_id,
        first_join,
        leave,
        while_out,
        rejoin,
    }
}

/// What the resident's `/backfill?v=<from>&limit=<limit>` answers: its timeline from the
/// earliest of `from` (included) backwards, newest first --
/// `hs_cli::federation::RegistryRoomSource::backfill`'s exact page.
fn resident_backfill(
    resident: &RoomActor<MemoryBackend>,
    from: &[OwnedEventId],
    limit: usize,
) -> Vec<Event> {
    let pos = from
        .iter()
        .filter_map(|id| resident.timeline_position(id))
        .min()
        .expect("the resident holds what is asked for");
    let (page, _) = resident.paginate(
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

/// Every page `/messages` would read backwards from the newest event, stopping at gaps.
fn read_back(actor: &RoomActor<MemoryBackend>) -> Vec<OwnedEventId> {
    let mut out = Vec::new();
    let mut from = None;
    loop {
        let page = actor.paginate_page(from, Direction::Backward, 4);
        out.extend(ids(page.events));
        if page.reached_edge {
            return out;
        }
        from = page.next;
    }
}

fn messages(events: &[Event]) -> impl Iterator<Item = &Event> {
    events
        .iter()
        .filter(|e| e.header().event_type == "m.room.message")
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
fn a_rejoin_through_another_server_opens_a_gap_that_a_backward_page_stops_at() {
    let r = rejoined(5);
    let leave_pos = r
        .joiner
        .timeline_position(r.leave.event_id())
        .expect("the leave is in the timeline");
    let top = r
        .joiner
        .timeline_position(r.rejoin.event_id())
        .expect("the rejoin is in the timeline");
    assert!(
        top > leave_pos + TIMELINE_GAP_SPAN,
        "positions are reserved below the rejoin: leave {leave_pos}, rejoin {top}"
    );

    let gaps = r.joiner.timeline_gaps();
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].top, top);
    assert_eq!(gaps[0].below, leave_pos);
    assert_eq!(gaps[0].filled_to, top);
    assert!(!gaps[0].closed);
    assert_eq!(
        gaps[0].missing,
        ids(r.while_out.last()),
        "the rejoin cites alice's last word, which b.example never saw"
    );

    // A backward page from the newest event stops at the gap instead of walking on into what
    // was held before the leave, and names its boundary.
    let page = r.joiner.paginate_page(None, Direction::Backward, 10);
    assert_eq!(ids(page.events), ids([&r.rejoin]));
    assert!(page.reached_edge);
    assert_eq!(page.gap, Some(top));
    assert_eq!(
        page.next,
        Some(PaginationToken::new(top, Direction::Backward))
    );

    // `paginate` (`/sync`, the federation endpoints) walks across, as before.
    let (across, _) = r.joiner.paginate(None, Direction::Backward, 10);
    assert_eq!(ids(across), ids([&r.rejoin, &r.leave, &r.first_join]));

    // Where to fetch from, and whom to ask.
    let anchor = r.joiner.gap_anchor(top).expect("the gap is open");
    assert_eq!(anchor.top, top);
    assert_eq!(anchor.from, ids(r.while_out.last()));
    assert_eq!(anchor.servers, vec!["a.example".to_owned()]);

    // A rejoin whose ancestors are all held opens nothing: bob leaves and comes straight back
    // before anybody else speaks.
    let mut joiner = r.joiner;
    let mut resident = r.resident;
    let leave_again = {
        // Bob's leave is made locally now that he is joined here again.
        joiner
            .membership_action(bob(), Action::Leave, bob(), json!({}), 1_000)
            .expect("bob leaves again")
    };
    assert!(matches!(
        resident.accept_remote_event(leave_again.clone()),
        Ok(RemoteEventOutcome::Stored(_))
    ));
    let straight_back = resident
        .membership_action(bob(), Action::Join, bob(), json!({}), 1_001)
        .expect("bob rejoins at once");
    let StateAtEvent { state, auth_chain } = join_snapshot(&resident, &leave_again);
    joiner
        .accept_remote_join_with_state(state, auth_chain, straight_back.clone())
        .expect("the rejoin is applied");
    assert_eq!(joiner.timeline_gaps().len(), 1, "no second gap");
    assert_eq!(
        joiner.timeline_position(straight_back.event_id()),
        joiner
            .timeline_position(leave_again.event_id())
            .map(|p| p + 1)
    );
}

#[test]
fn what_happened_while_out_is_placed_between_the_leave_and_the_rejoin() {
    let mut r = rejoined(5);
    let top = r
        .joiner
        .timeline_position(r.rejoin.event_id())
        .expect("held");
    let mut published = r.joiner.subscribe();

    let anchor = r.joiner.gap_anchor(top).expect("the gap is open");
    let batch = resident_backfill(&r.resident, &anchor.from, 100);
    // The resident's walk goes on past the leave to the room's creation: everything it holds.
    assert!(batch.len() > r.while_out.len() + 2);
    let fill = r
        .joiner
        .accept_gap_events(top, batch.clone())
        .expect("the batch is stored");
    assert_eq!(
        fill.added,
        r.while_out.len(),
        "five messages and the rename"
    );
    assert!(fill.closed, "everything the gap cited is held now");
    assert!(r.joiner.timeline_gaps()[0].closed);
    assert!(r.joiner.gap_anchor(top).is_none());

    // --- reading back from the rejoin walks the gap in the resident's order, then the leave
    // and what was held before it ---
    let mut expected: Vec<OwnedEventId> = vec![r.rejoin.event_id().to_owned()];
    expected.extend(ids(r.while_out.iter().rev()));
    expected.extend(ids([&r.leave, &r.first_join]));
    assert_eq!(read_back(&r.joiner), expected);
    let (across, _) = r.joiner.paginate(None, Direction::Backward, 100);
    assert_eq!(ids(across), expected);

    // --- nothing older than the leave was put in the gap: the creation events (held as
    // outliers since the first join) and alice's word before bob came are not placed ---
    let create = r
        .resident
        .state_event("m.room.create", "")
        .expect("state")
        .expect("created")
        .clone();
    assert_eq!(r.joiner.timeline_position(create.event_id()), None);

    // --- the state at each message is the resident's: before the rename, the name it had then
    // (none); after it, the new one. (The rename itself was held as an outlier since the rejoin
    // brought it, and keeps an outlier's state, as `accept_backfilled_events` documents.) ---
    for event in messages(&r.while_out) {
        assert_eq!(
            state_at(&r.joiner, event),
            state_at(&r.resident, event),
            "state at {}",
            event.event_id()
        );
    }

    // --- bob may read it: `shared` history ---
    for event in &r.while_out {
        assert!(
            r.joiner
                .event_visible_to(event, &bob())
                .expect("visibility")
        );
    }

    // --- history is not news: nothing published, and a follower with a cursor at the leave
    // sees only the rejoin ---
    assert!(
        matches!(
            published.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "a gap event must not be published as an update"
    );
    let leave_pos = r
        .joiner
        .timeline_position(r.leave.event_id())
        .expect("held");
    assert_eq!(
        ids(r
            .joiner
            .events_after(leave_pos, 100)
            .into_iter()
            .map(|(_, e)| e)),
        ids([&r.rejoin])
    );
    assert_eq!(
        ids(r.joiner.events_after(0, 100).into_iter().map(|(_, e)| e)),
        ids([&r.first_join, &r.leave, &r.rejoin])
    );

    // --- the same batch again adds nothing ---
    let again = r
        .joiner
        .accept_gap_events(top, batch)
        .expect("stored again");
    assert_eq!(again.added, 0);

    // --- and a reload from the store comes back the same ---
    let reloaded = RoomActor::load(r.backend, r.tables, r.identity, &r.room_id)
        .expect("load")
        .expect("the room exists");
    assert_eq!(read_back(&reloaded), expected);
    assert_eq!(reloaded.timeline_gaps(), r.joiner.timeline_gaps());
    assert_eq!(
        ids(reloaded.events_after(0, 100).into_iter().map(|(_, e)| e)),
        ids([&r.first_join, &r.leave, &r.rejoin])
    );
    for event in messages(&r.while_out) {
        assert_eq!(state_at(&reloaded, event), state_at(&r.resident, event));
    }
}

#[test]
fn a_gap_larger_than_one_batch_is_filled_a_batch_at_a_time_and_survives_a_reload() {
    let mut r = rejoined(8);
    let top = r
        .joiner
        .timeline_position(r.rejoin.event_id())
        .expect("held");

    // The resident answers four at a time.
    let anchor = r.joiner.gap_anchor(top).expect("open");
    let first = resident_backfill(&r.resident, &anchor.from, 4);
    let fill = r.joiner.accept_gap_events(top, first).expect("first batch");
    assert_eq!(fill.added, 4);
    assert!(!fill.closed, "five more events are missing");
    let newest_four: Vec<OwnedEventId> = ids(r.while_out.iter().rev().take(4));
    let gap = &r.joiner.timeline_gaps()[0];
    assert_eq!(gap.filled_to, top - 4);
    assert_eq!(
        gap.missing,
        ids([&r.while_out[r.while_out.len() - 5]]),
        "the next fetch walks back from what the oldest placed event cites"
    );

    // A page from the rejoin reads the four and stops at the gap again, with a token.
    let page = r.joiner.paginate_page(None, Direction::Backward, 10);
    let mut expected = vec![r.rejoin.event_id().to_owned()];
    expected.extend(newest_four);
    assert_eq!(ids(page.events), expected);
    assert_eq!(page.gap, Some(top));
    assert_eq!(
        page.next,
        Some(PaginationToken::new(top - 4, Direction::Backward))
    );

    // A reload in between finds the same gap, as far filled and lacking the same event.
    let reloaded = RoomActor::load(
        r.backend.clone(),
        r.tables.clone(),
        r.identity.clone(),
        &r.room_id,
    )
    .expect("load")
    .expect("the room exists");
    assert_eq!(reloaded.timeline_gaps(), r.joiner.timeline_gaps());
    drop(reloaded);

    // The second batch walks back from there and closes the gap.
    let anchor = r.joiner.gap_anchor(top).expect("still open");
    let second = resident_backfill(&r.resident, &anchor.from, 100);
    let fill = r
        .joiner
        .accept_gap_events(top, second)
        .expect("second batch");
    assert_eq!(fill.added, r.while_out.len() - 4);
    assert!(fill.closed);
    let mut expected = vec![r.rejoin.event_id().to_owned()];
    expected.extend(ids(r.while_out.iter().rev()));
    expected.extend(ids([&r.leave, &r.first_join]));
    assert_eq!(read_back(&r.joiner), expected);
    for event in messages(&r.while_out) {
        assert_eq!(
            state_at(&r.joiner, event),
            state_at(&r.resident, event),
            "state at {}",
            event.event_id()
        );
    }
}

#[test]
fn an_answer_with_nothing_new_closes_the_gap_and_a_page_walks_across_it() {
    let mut r = rejoined(3);
    let top = r
        .joiner
        .timeline_position(r.rejoin.event_id())
        .expect("held");
    // The resident answers with only what b.example already holds.
    let fill = r
        .joiner
        .accept_gap_events(top, vec![r.leave.clone(), r.first_join.clone()])
        .expect("stored");
    assert_eq!(fill.added, 0);
    assert!(fill.closed);
    let page = r.joiner.paginate_page(None, Direction::Backward, 10);
    assert_eq!(page.gap, None);
    assert_eq!(ids(page.events), ids([&r.rejoin, &r.leave, &r.first_join]));
}

// ---------------------------------------------------------------------------------------------
// The same, through `GET /messages`: what a client reading back from the rejoin is answered.
// ---------------------------------------------------------------------------------------------

/// A `Backfill` that answers from the resident's own timeline, `batch` events a time, as
/// `hs_cli::backfill::FederationBackfill` does over `/backfill` -- or fails, standing in for a
/// resident that cannot be reached.
struct FromResident {
    resident: Mutex<RoomActor<MemoryBackend>>,
    rooms: Arc<RoomRegistry<MemoryBackend>>,
    batch: usize,
    reachable: bool,
    gap_fetches: AtomicUsize,
}

#[async_trait::async_trait]
impl Backfill for FromResident {
    async fn backfill(&self, _room_id: &RoomId) -> Result<usize, RoomError> {
        // History from before bob's first join is not what these tests are about.
        Ok(0)
    }

    async fn fill_gap(&self, room_id: &RoomId, top: i64) -> Result<usize, RoomError> {
        self.gap_fetches.fetch_add(1, Ordering::SeqCst);
        if !self.reachable {
            return Err(RoomError::BackfillFailed(
                "a.example: connection refused".to_owned(),
            ));
        }
        let handle = self.rooms.get_or_load(room_id).await?;
        let Some(anchor) = handle.query(move |actor| actor.gap_anchor(top)).await else {
            return Ok(0);
        };
        let batch = {
            let resident = self.resident.lock().expect("the resident's lock");
            resident_backfill(&resident, &anchor.from, self.batch)
        };
        Ok(handle.accept_gap_events(top, batch).await?.added)
    }
}

/// What the `/messages` tests expect to read, in the order alice's server has it.
struct Expected {
    room_id: OwnedRoomId,
    rejoin: OwnedEventId,
    while_out: Vec<OwnedEventId>,
    leave: OwnedEventId,
    first_join: OwnedEventId,
}

/// b.example's room layer over the store `rejoined` left (the registry loads the room from it),
/// with a [`FromResident`] hook installed.
fn room_layer(
    r: Rejoined,
    batch: usize,
    reachable: bool,
) -> (RoomState<MemoryBackend>, Arc<FromResident>, Expected) {
    let expected = Expected {
        room_id: r.room_id.clone(),
        rejoin: r.rejoin.event_id().to_owned(),
        while_out: ids(&r.while_out),
        leave: r.leave.event_id().to_owned(),
        first_join: r.first_join.event_id().to_owned(),
    };
    let rooms = Arc::new(
        RoomRegistry::open(r.backend.clone(), r.identity.clone()).expect("open the registry"),
    );
    let hook = Arc::new(FromResident {
        resident: Mutex::new(r.resident),
        rooms: rooms.clone(),
        batch,
        reachable,
        gap_fetches: AtomicUsize::new(0),
    });
    rooms.install_backfill(hook.clone());
    let state = RoomState {
        auth: hs_auth::state::AuthState::in_memory(),
        rooms,
        identity: r.identity,
        remote_join: None,
    };
    (state, hook, expected)
}

/// Reads `/messages` backwards from the live end, `limit` at a time, until there is no `end`.
/// Returns every event ID in the order read.
async fn read_messages(
    state: &RoomState<MemoryBackend>,
    room_id: &RoomId,
    limit: usize,
) -> Vec<OwnedEventId> {
    let mut from: Option<String> = None;
    let mut out = Vec::new();
    for _ in 0..20 {
        let response = get_messages(
            State(state.clone()),
            Path(room_id.to_string()),
            Query(MessagesQuery {
                filter: None,
                to: None,
                from: from.clone(),
                dir: Some("b".to_owned()),
                limit: Some(limit),
            }),
            RoomRequester(Requester {
                user_id: bob(),
                device_id: None,
                is_guest: false,
                is_admin: false,
                shadow_banned: false,
                suspended: false,
                appservice: None,
                access_token_id: None,
            }),
        )
        .await
        .expect("/messages answers");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        for event in body["chunk"].as_array().expect("a chunk") {
            let id = event["event_id"].as_str().expect("an event ID");
            out.push(OwnedEventId::try_from(id).expect("a valid event ID"));
        }
        match body.get("end").and_then(serde_json::Value::as_str) {
            Some(end) => from = Some(end.to_owned()),
            None => return out,
        }
    }
    panic!("still paginating after 20 requests: {out:?}");
}

#[tokio::test]
async fn messages_read_back_from_a_rejoin_reach_what_happened_while_out_then_what_was_held() {
    let (state, hook, e) = room_layer(rejoined(8), 4, true);
    let read = read_messages(&state, &e.room_id, 3).await;

    let mut expected = vec![e.rejoin.clone()];
    expected.extend(e.while_out.iter().rev().cloned());
    expected.extend([e.leave.clone(), e.first_join.clone()]);
    assert_eq!(
        read, expected,
        "the rejoin, the nine events of the gap newest first, then the leave and the join"
    );
    assert_eq!(
        hook.gap_fetches.load(Ordering::SeqCst),
        3,
        "nine missed events, four a fetch"
    );

    // Read again: nothing more is fetched, and the answer is the same.
    assert_eq!(read_messages(&state, &e.room_id, 50).await, read);
    assert_eq!(hook.gap_fetches.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn messages_read_on_past_a_gap_that_cannot_be_filled() {
    let (state, hook, e) = room_layer(rejoined(3), 100, false);
    assert_eq!(
        read_messages(&state, &e.room_id, 10).await,
        vec![e.rejoin, e.leave, e.first_join],
        "with the resident unreachable, the client still reaches what was held before the leave"
    );
    assert_eq!(hook.gap_fetches.load(Ordering::SeqCst), 1);
}

#[test]
fn a_gap_filled_with_the_state_asked_of_the_resident_has_its_state_at_every_event() {
    use hs_room::backfill::{FetchedState, HistoryKind, StateSource};

    let mut r = rejoined(5);
    let top = r
        .joiner
        .timeline_position(r.rejoin.event_id())
        .expect("the rejoin is in the timeline");
    let anchor = r.joiner.gap_anchor(top).expect("open");
    // Four back from alice's last word: three messages and the rename.
    let batch = resident_backfill(&r.resident, &anchor.from, 4);
    let kind = HistoryKind::Gap { top };
    let plan = r
        .joiner
        .plan_history(kind, &batch)
        .expect("plan")
        .expect("something to place");
    assert_eq!(
        Some(plan.oldest.clone()),
        r.while_out
            .iter()
            .find(|e| e.header().event_type == "m.room.name")
            .map(|e| e.event_id().to_owned()),
        "the oldest of the four is the rename"
    );

    // The resident's `/state_ids` at the rename, and whatever of it b.example lacks.
    let StateAtEvent { state, auth_chain } = r
        .resident
        .state_before_event(&plan.oldest)
        .expect("lookup")
        .expect("known");
    let state_ids: Vec<OwnedEventId> = state.iter().map(|e| e.event_id().to_owned()).collect();
    let mut wanted = state_ids.clone();
    wanted.extend(auth_chain.iter().map(|e| e.event_id().to_owned()));
    let events = r
        .joiner
        .events_not_held(&wanted)
        .iter()
        .filter_map(|id| r.resident.event_by_id(id).cloned())
        .collect();
    let outcome = r
        .joiner
        .accept_history(
            kind,
            batch.clone(),
            Some(FetchedState {
                at: plan.oldest,
                state_ids,
                events,
            }),
        )
        .expect("placed");
    assert_eq!(outcome.state, StateSource::Fetched);
    assert_eq!(outcome.added, 4);
    assert_eq!(outcome.rejected, 0);
    assert!(!outcome.gap_closed, "two messages are still missing");

    // The state at each placed event is the resident's: bob is out, the room renamed from the
    // rename on.
    for event in &batch {
        assert_eq!(
            state_at(&r.joiner, event),
            state_at(&r.resident, event),
            "state at {}",
            event.event_id()
        );
    }
}
