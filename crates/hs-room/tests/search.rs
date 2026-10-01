//! `POST /search` over real HTTP (the `hs-testkit` scenario router, as `scenario.rs`), with the
//! search indexer (`hs_room::search::run_indexer`) running in the background as `hs serve` runs
//! it: what a requester finds is what they may see, in the order and pages they ask for.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::Method;
use hs_auth::state::AuthState;
use hs_kv::memory::MemoryBackend;
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use hs_room::state::RoomState;
use hs_testkit::Scenario;
use serde_json::{Value, json};

fn app(backend: MemoryBackend) -> (axum::Router, Arc<RoomRegistry<MemoryBackend>>) {
    let auth_state = AuthState::in_memory();
    let identity = HomeserverIdentity::for_tests("example.org");
    let registry = Arc::new(RoomRegistry::open(backend, identity.clone()).expect("open registry"));
    let room_state = RoomState {
        auth: auth_state.clone(),
        rooms: registry.clone(),
        identity,
        remote_join: None,
    };
    let (room_router, _manifest) = hs_room::routes::router::<MemoryBackend>();
    let router = hs_auth::routes::router()
        .with_state(auth_state)
        .merge(room_router.with_state(room_state));
    (router, registry)
}

async fn create(scenario: &mut Scenario, who: &str, body: Value) -> String {
    let created = scenario
        .send(Some(who), Method::POST, "/createRoom", Some(body))
        .await;
    created.assert_ok();
    created.str_field("room_id").to_owned()
}

async fn say(scenario: &mut Scenario, who: &str, room: &str, body: &str) -> String {
    let txn = format!("t{}", rand_suffix());
    let sent = scenario
        .send(
            Some(who),
            Method::PUT,
            &format!("/rooms/{room}/send/m.room.message/{txn}"),
            Some(json!({"msgtype": "m.text", "body": body})),
        )
        .await;
    sent.assert_ok();
    // Distinct timestamps, so that `recent` has one answer.
    tokio::time::sleep(Duration::from_millis(5)).await;
    sent.str_field("event_id").to_owned()
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

async fn search(scenario: &mut Scenario, who: &str, criteria: Value, next: Option<&str>) -> Value {
    let path = match next {
        Some(token) => format!("/search?next_batch={token}"),
        None => "/search".to_owned(),
    };
    let response = scenario
        .send(
            Some(who),
            Method::POST,
            &path,
            Some(json!({"search_categories": {"room_events": criteria}})),
        )
        .await;
    response.assert_ok();
    response.json["search_categories"]["room_events"].clone()
}

fn ids(room_events: &Value) -> Vec<String> {
    room_events["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["result"]["event_id"].as_str().unwrap().to_owned())
        .collect()
}

/// Searches until `count` is `want` (the indexer runs behind the writes), or panics.
async fn search_until(scenario: &mut Scenario, who: &str, criteria: Value, want: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = search(scenario, who, criteria.clone(), None).await;
        if found["count"].as_u64() == Some(want) {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "{who}'s search {criteria} never counted {want}: {found}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_finds_only_what_the_requester_may_see_ordered_paged_and_in_context() {
    let backend = MemoryBackend::new();
    let (router, registry) = app(backend);
    tokio::spawn(hs_room::search::run_indexer(registry.clone()));
    let mut s = Scenario::new(router);
    s.register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    s.register("bob", "bob", "hunter2official")
        .await
        .assert_ok();

    // Room 1: alice's, public, history visible only from a member's join.
    let shared = create(
        &mut s,
        "alice",
        json!({
            "preset": "public_chat",
            "initial_state": [{"type": "m.room.history_visibility", "state_key": "",
                               "content": {"history_visibility": "joined"}}],
        }),
    )
    .await;
    let early = say(&mut s, "alice", &shared, "the pineapple is ripe, early").await;
    s.send(
        Some("bob"),
        Method::POST,
        &format!("/rooms/{shared}/join"),
        Some(json!({})),
    )
    .await
    .assert_ok();
    let later = say(&mut s, "alice", &shared, "a pineapple, later").await;
    let party = say(
        &mut s,
        "bob",
        &shared,
        "PINEAPPLE pineapple pineapple party",
    )
    .await;
    say(&mut s, "alice", &shared, "nothing to see here").await;
    // Room 2: alice's alone.
    let secret_room = create(&mut s, "alice", json!({"preset": "private_chat"})).await;
    let secret = say(&mut s, "alice", &secret_room, "pineapple secret").await;
    // Room 3: bob's alone, named.
    let own = create(
        &mut s,
        "bob",
        json!({"preset": "private_chat", "name": "Pineapple Club"}),
    )
    .await;
    let mine = say(&mut s, "bob", &own, "a pineapple of my own").await;

    // Bob: the two messages after his join in room 1, and room 3's message and name. Not the
    // message from before his join (`joined` visibility), not alice's room.
    let recent = search_until(
        &mut s,
        "bob",
        json!({"search_term": "pineapple", "order_by": "recent"}),
        4,
    )
    .await;
    let found = ids(&recent);
    assert!(
        !found.contains(&early) && !found.contains(&secret),
        "{found:?}"
    );
    assert_eq!(found[0], mine, "newest first");
    assert_eq!(&found[2..], [party.clone(), later.clone()]);
    assert_eq!(
        recent["results"][1]["result"]["type"], "m.room.name",
        "the room's name, set at creation before bob's message"
    );
    assert!(
        recent["highlights"]
            .as_array()
            .unwrap()
            .contains(&json!("pineapple"))
    );

    // By rank, the message that says it three times is first.
    let ranked = search(&mut s, "bob", json!({"search_term": "pineapple"}), None).await;
    assert_eq!(ids(&ranked)[0], party);
    assert!(
        ranked["results"][0]["rank"].as_f64().unwrap()
            > ranked["results"][1]["rank"].as_f64().unwrap()
    );

    // Alice: her three messages in room 1 and her secret; nothing of bob's room.
    let alice_found = search_until(&mut s, "alice", json!({"search_term": "pineapple"}), 4).await;
    let alice_ids = ids(&alice_found);
    for id in [&early, &later, &party, &secret] {
        assert!(alice_ids.contains(id), "{alice_ids:?}");
    }

    // keys, filter.rooms, a prefix, and every word required.
    let bodies = search(
        &mut s,
        "bob",
        json!({"search_term": "pineapple", "keys": ["content.body"]}),
        None,
    )
    .await;
    assert_eq!(bodies["count"], 3);
    let in_own = search(
        &mut s,
        "bob",
        json!({"search_term": "pineapple", "filter": {"rooms": [own.clone()]}}),
        None,
    )
    .await;
    assert_eq!(in_own["count"], 2);
    let prefix = search(&mut s, "bob", json!({"search_term": "pine"}), None).await;
    assert_eq!(prefix["count"], 4);
    let both = search(
        &mut s,
        "bob",
        json!({"search_term": "pineapple party"}),
        None,
    )
    .await;
    assert_eq!(ids(&both), std::slice::from_ref(&party));

    // Pages of one, by `next_batch`, are the whole answer in order, once each.
    let mut paged = Vec::new();
    let mut next: Option<String> = None;
    loop {
        let page = search(
            &mut s,
            "bob",
            json!({"search_term": "pineapple", "order_by": "recent", "filter": {"limit": 1}}),
            next.as_deref(),
        )
        .await;
        assert_eq!(page["count"], 4, "count is the whole answer on every page");
        paged.extend(ids(&page));
        match page["next_batch"].as_str() {
            Some(token) => next = Some(token.to_owned()),
            None => break,
        }
    }
    assert_eq!(paged, found);

    // Context, profiles, state and grouping.
    let with_context = search(
        &mut s,
        "bob",
        json!({
            "search_term": "later",
            "event_context": {"before_limit": 1, "after_limit": 1, "include_profile": true},
            "include_state": true,
            "groupings": {"group_by": [{"key": "room_id"}]},
        }),
        None,
    )
    .await;
    assert_eq!(ids(&with_context), std::slice::from_ref(&later));
    let context = &with_context["results"][0]["context"];
    assert_eq!(
        context["events_before"][0]["type"], "m.room.member",
        "bob's join is just before it: {context}"
    );
    assert_eq!(context["events_after"][0]["event_id"], party.as_str());
    assert!(context["profile_info"]["@alice:example.org"].is_object());
    assert!(context["start"].is_string() && context["end"].is_string());
    assert!(
        with_context["state"][shared.as_str()]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "m.room.create")
    );
    assert_eq!(
        with_context["groups"]["room_id"][shared.as_str()]["results"],
        json!([later.clone()])
    );

    // A redacted message is no longer found.
    s.send(
        Some("bob"),
        Method::PUT,
        &format!("/rooms/{shared}/redact/{party}/r1"),
        Some(json!({})),
    )
    .await
    .assert_ok();
    search_until(&mut s, "bob", json!({"search_term": "party"}), 0).await;

    // A malformed request is refused.
    let bad = s
        .send(
            Some("bob"),
            Method::POST,
            "/search",
            Some(json!({"search_categories": {"room_events": {"search_term": "x", "order_by": "nope"}}})),
        )
        .await;
    assert_eq!(bad.status, axum::http::StatusCode::BAD_REQUEST);
}

/// The index is durable and resumes from its cursors: what was written while no indexer ran
/// (a server down, a crash) is indexed by the next one, and nothing is indexed twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_a_restart_the_index_resumes_from_its_cursors() {
    let backend = MemoryBackend::new();
    let (router, registry) = app(backend.clone());
    let indexer = tokio::spawn(hs_room::search::run_indexer(registry.clone()));
    let mut s = Scenario::new(router);
    s.register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let room = create(&mut s, "alice", json!({"preset": "private_chat"})).await;
    for n in 0..5 {
        say(&mut s, "alice", &room, &format!("durable note {n}")).await;
    }
    search_until(&mut s, "alice", json!({"search_term": "durable"}), 5).await;
    let documents = registry.search_index().documents().unwrap();
    assert_eq!(documents, 5);

    // The indexer stops; two more messages are written with nobody indexing.
    indexer.abort();
    let _ = indexer.await;
    let room_id = ruma::RoomId::parse(&room).unwrap();
    let handle = registry.get_or_load(&room_id).await.unwrap();
    for n in 5..7 {
        handle
            .send_event(
                ruma::UserId::parse("@alice:example.org").unwrap(),
                "m.room.message".to_owned(),
                None,
                json!({"msgtype": "m.text", "body": format!("durable note {n}")}),
                None,
                1_700_000_000_000 + n,
            )
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(registry.search_index().documents().unwrap(), 5);

    // A new registry and indexer over the same store, as after a restart: the two are indexed,
    // the five are not indexed again.
    let (_router, restarted) = app(backend);
    tokio::spawn(hs_room::search::run_indexer(restarted.clone()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while restarted.search_index().documents().unwrap() < 7 {
        assert!(
            Instant::now() < deadline,
            "the restarted indexer never caught up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(restarted.search_index().documents().unwrap(), 7);
}
