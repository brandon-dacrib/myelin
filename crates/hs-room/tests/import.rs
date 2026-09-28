//! Importing a room's history from another implementation's database for this same server (the
//! Synapse importer): `RoomRegistry::import_shell`, `RoomActorHandle::import_event` and
//! `RoomActorHandle::import_redaction`.
//!
//! One backend stands in for Synapse's history (a room built here, whose events are read back
//! out), another for this server; the events are imported in order, exactly as the importer
//! copies them.

use std::sync::Arc;

use hs_kv::memory::MemoryBackend;
use hs_model::Event;
use hs_room::RoomError;
use hs_room::actor::{CreateRoomRequest, RemoteEventOutcome, RoomActor};
use hs_room::identity::HomeserverIdentity;
use hs_room::persist::Tables;
use hs_room::registry::RoomRegistry;
use ruma::{OwnedUserId, RoomVersionId, user_id};
use serde_json::json;

fn alice() -> OwnedUserId {
    user_id!("@alice:old.example").to_owned()
}

/// A room with a name, two messages and a redaction of the second, as the other implementation
/// held it: every event, in the order it was stored, the redacted event's id, and the room id.
fn history() -> (Vec<Event>, ruma::OwnedEventId, ruma::OwnedRoomId) {
    let backend = MemoryBackend::new();
    let tables = Tables::open(&backend).expect("open tables");
    let mut actor = RoomActor::create_room(
        backend,
        tables,
        HomeserverIdentity::for_tests("old.example"),
        alice(),
        CreateRoomRequest {
            preset: Some("private_chat".to_owned()),
            room_version: Some(RoomVersionId::V11),
            name: Some("history".to_owned()),
            ..Default::default()
        },
        1,
    )
    .expect("create the room");
    actor
        .send_event(
            alice(),
            "m.room.message".into(),
            None,
            json!({"msgtype": "m.text", "body": "kept"}),
            None,
            2,
        )
        .expect("first message");
    let oops = actor
        .send_event(
            alice(),
            "m.room.message".into(),
            None,
            json!({"msgtype": "m.text", "body": "redacted later"}),
            None,
            3,
        )
        .expect("second message");
    actor
        .send_event(
            alice(),
            "m.room.redaction".into(),
            None,
            json!({"redacts": oops.event_id()}),
            Some(oops.event_id().to_owned()),
            4,
        )
        .expect("redaction");
    let events = actor
        .events_after(0, 1000)
        .into_iter()
        .map(|(_, e)| e.clone())
        .collect();
    (
        events,
        oops.event_id().to_owned(),
        actor.room_id().to_owned(),
    )
}

#[tokio::test]
async fn an_imported_room_is_stored_whole_announced_to_nobody_and_survives_a_reload() {
    let (events, redacted, room_id) = history();
    let backend = MemoryBackend::new();
    let rooms = Arc::new(
        RoomRegistry::open(
            backend.clone(),
            HomeserverIdentity::for_tests("old.example"),
        )
        .expect("open registry"),
    );
    let mut updates = rooms.subscribe_global();

    let handle = rooms
        .import_shell(&room_id, RoomVersionId::V11)
        .await
        .expect("shell");
    for event in &events {
        let outcome = handle.import_event(event.clone()).await.expect("import");
        assert!(matches!(outcome, RemoteEventOutcome::Stored(_)));
    }
    handle
        .import_redaction(redacted.clone())
        .await
        .expect("redaction applied");

    // Nothing was published: not for the events, nor for the shell.
    assert!(
        updates.try_recv().is_err(),
        "an import must not announce its events"
    );

    // A second pass (an import resumed after a stop) finds every event already there.
    let again = rooms
        .import_shell(&room_id, RoomVersionId::V11)
        .await
        .expect("existing room");
    for event in &events {
        assert!(matches!(
            again.import_event(event.clone()).await.expect("re-import"),
            RemoteEventOutcome::AlreadyKnown
        ));
    }

    // What was imported is what a fresh load of the store finds, redaction included.
    let tables = Tables::open(&backend).expect("tables");
    let loaded = RoomActor::load(
        backend.clone(),
        tables,
        HomeserverIdentity::for_tests("old.example"),
        &room_id,
    )
    .expect("load")
    .expect("the room exists");
    let ids: Vec<_> = loaded
        .events_after(0, 1000)
        .into_iter()
        .map(|(_, e)| e.event_id().to_owned())
        .collect();
    let original: Vec<_> = events.iter().map(|e| e.event_id().to_owned()).collect();
    assert_eq!(ids, original);
    assert!(
        loaded
            .event_by_id(&redacted)
            .expect("held")
            .header()
            .flags
            .is_redacted()
    );
    let name = loaded
        .state_event("m.room.name", "")
        .expect("state")
        .expect("named");
    let original_name = events
        .iter()
        .find(|e| e.header().event_type == "m.room.name")
        .expect("the room was named");
    assert_eq!(name.event_id(), original_name.event_id());
    assert!(loaded.head_update().is_some());
}

#[tokio::test]
async fn a_shell_whose_first_event_is_refused_is_forgotten() {
    let (events, _, room_id) = history();
    let rooms = Arc::new(
        RoomRegistry::open(
            MemoryBackend::new(),
            HomeserverIdentity::for_tests("old.example"),
        )
        .expect("open registry"),
    );
    let handle = rooms
        .import_shell(&room_id, RoomVersionId::V11)
        .await
        .expect("shell");
    // The second event cites the create event, which this shell does not hold.
    let refused = handle.import_event(events[1].clone()).await;
    assert!(matches!(refused, Err(RoomError::MissingAncestors(_))));
    rooms.discard_import_shell(&room_id, &handle).await;
    assert!(matches!(
        rooms.get_or_load(&room_id).await,
        Err(RoomError::RoomNotFound(_))
    ));
}
