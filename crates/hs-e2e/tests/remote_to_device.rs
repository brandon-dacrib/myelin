//! To-device messages across servers (`hs_e2e::federation`): `/sendToDevice` hands the messages
//! for users of other servers to the installed `ToDeviceOutbox`, one `m.direct_to_device` EDU per
//! server, and an `m.direct_to_device` EDU from another server reaches this server's devices
//! once per `message_id`, and only when its sender is a user of the server it came from.

use std::sync::{Arc, Mutex};

use axum::http::Method;
use hs_auth::state::AuthState;
use hs_e2e::federation::{InboundToDevice, ToDeviceOutbox, receive_direct_to_device};
use hs_e2e::state::E2eState;
use hs_e2e::store::tables::TablesE2eStore;
use hs_kv::memory::MemoryBackend;
use hs_testkit::Scenario;
use serde_json::{Value, json};

/// Records every EDU it is handed, with its destination.
#[derive(Default)]
struct RecordingOutbox(Mutex<Vec<(String, Value)>>);

impl ToDeviceOutbox for RecordingOutbox {
    fn send_direct_to_device(&self, destination: &str, content: Value) {
        self.0
            .lock()
            .unwrap()
            .push((destination.to_owned(), content));
    }
}

fn app(outbox: Option<Arc<RecordingOutbox>>) -> (axum::Router, E2eState<MemoryBackend>) {
    let auth_state = AuthState::in_memory();
    let store = TablesE2eStore::open(MemoryBackend::new()).expect("open e2e store");
    let e2e_state: E2eState<MemoryBackend> = E2eState::new(auth_state.clone(), Arc::new(store));
    if let Some(outbox) = outbox {
        e2e_state.install_to_device_outbox(outbox);
    }
    let (e2e_router, _manifest) = hs_e2e::routes::router::<MemoryBackend>();
    (
        hs_auth::routes::router()
            .with_state(auth_state)
            .merge(e2e_router.with_state(e2e_state.clone())),
        e2e_state,
    )
}

/// Registers alice and returns her user ID and device ID.
async fn alice(scenario: &mut Scenario) -> (String, String) {
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let session = scenario.session("alice").unwrap();
    (
        session.user_id.clone().unwrap(),
        session.device_id.clone().unwrap(),
    )
}

/// The to-device messages waiting for `user`'s `device`: `(sender, type, content)`.
async fn inbox(state: &E2eState<MemoryBackend>, user: &str, device: &str) -> Vec<Value> {
    let user = ruma::UserId::parse(user).unwrap();
    let device: &ruma::DeviceId = device.into();
    let (messages, _) = state.store.poll_since(&user, device, 0, 100).await.unwrap();
    messages
        .into_iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect()
}

#[tokio::test]
async fn messages_for_other_servers_go_to_the_outbox_one_edu_per_server() {
    let outbox = Arc::new(RecordingOutbox::default());
    let (router, state) = app(Some(outbox.clone()));
    let mut scenario = Scenario::new(router);
    let (alice_id, alice_device) = alice(&mut scenario).await;

    scenario
        .send(
            Some("alice"),
            Method::PUT,
            "/sendToDevice/m.room_key_request/txn1",
            Some(json!({"messages": {
                "@bob:there.example": {"BOBDEV": {"n": 1}},
                "@carol:there.example": {"*": {"n": 2}},
                "@dan:else.example": {"DANDEV": {"n": 3}},
                alice_id.clone(): {alice_device.clone(): {"n": 4}},
            }})),
        )
        .await
        .assert_ok();

    let mut sent = outbox.0.lock().unwrap().clone();
    sent.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(sent.len(), 2, "one EDU per server: {sent:?}");
    assert_eq!(sent[0].0, "else.example");
    assert_eq!(sent[1].0, "there.example");
    let there = &sent[1].1;
    assert_eq!(there["sender"], alice_id.as_str());
    assert_eq!(there["type"], "m.room_key_request");
    assert_eq!(
        there["messages"],
        json!({
            "@bob:there.example": {"BOBDEV": {"n": 1}},
            "@carol:there.example": {"*": {"n": 2}},
        })
    );
    let ids: Vec<&str> = sent
        .iter()
        .map(|(_, c)| c["message_id"].as_str().unwrap())
        .collect();
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert_ne!(ids[0], ids[1], "each EDU has its own message_id");

    // The local recipient's message stayed here.
    let local = inbox(&state, &alice_id, &alice_device).await;
    assert_eq!(local.len(), 1, "{local:?}");

    // A retried request is not sent again.
    scenario
        .send(
            Some("alice"),
            Method::PUT,
            "/sendToDevice/m.room_key_request/txn1",
            Some(json!({"messages": {"@bob:there.example": {"BOBDEV": {"n": 1}}}})),
        )
        .await
        .assert_ok();
    assert_eq!(outbox.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn without_an_outbox_messages_for_other_servers_are_dropped_and_the_request_succeeds() {
    let (router, _state) = app(None);
    let mut scenario = Scenario::new(router);
    alice(&mut scenario).await;
    scenario
        .send(
            Some("alice"),
            Method::PUT,
            "/sendToDevice/m.test/txn1",
            Some(json!({"messages": {"@bob:there.example": {"BOBDEV": {}}}})),
        )
        .await
        .assert_ok();
}

#[tokio::test]
async fn an_inbound_message_reaches_every_named_device_once_per_message_id() {
    let (router, state) = app(None);
    let mut scenario = Scenario::new(router);
    let (alice_id, alice_device) = alice(&mut scenario).await;

    let edu = |message_id: &str| {
        json!({
            "sender": "@bob:there.example",
            "type": "m.room.encrypted",
            "message_id": message_id,
            "messages": {
                alice_id.clone(): {"*": {"ciphertext": message_id}},
                "@zed:third.example": {"Z": {"ciphertext": "not ours"}},
            },
        })
    };
    assert_eq!(
        receive_direct_to_device(&state, "there.example", &edu("m1"))
            .await
            .unwrap(),
        InboundToDevice::Delivered { messages: 1 }
    );
    // The same message_id again -- a transaction the other server retried -- is not delivered
    // again.
    assert_eq!(
        receive_direct_to_device(&state, "there.example", &edu("m1"))
            .await
            .unwrap(),
        InboundToDevice::Duplicate
    );
    let waiting = inbox(&state, &alice_id, &alice_device).await;
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0]["sender"], "@bob:there.example");
    assert_eq!(waiting[0]["event_type"], "m.room.encrypted");
    assert_eq!(waiting[0]["content"], json!({"ciphertext": "m1"}));

    // A new message_id is a new message.
    assert_eq!(
        receive_direct_to_device(&state, "there.example", &edu("m2"))
            .await
            .unwrap(),
        InboundToDevice::Delivered { messages: 1 }
    );
    assert_eq!(inbox(&state, &alice_id, &alice_device).await.len(), 2);
}

#[tokio::test]
async fn an_inbound_message_that_speaks_for_another_server_or_has_no_message_id_is_dropped() {
    let (router, state) = app(None);
    let mut scenario = Scenario::new(router);
    let (alice_id, alice_device) = alice(&mut scenario).await;
    let messages = json!({alice_id.clone(): {alice_device.clone(): {"x": 1}}});

    let forged = json!({
        "sender": "@bob:there.example", "type": "m.test", "message_id": "m1",
        "messages": messages,
    });
    assert!(matches!(
        receive_direct_to_device(&state, "evil.example", &forged)
            .await
            .unwrap(),
        InboundToDevice::Dropped(_)
    ));
    let no_id = json!({"sender": "@bob:there.example", "type": "m.test", "messages": messages});
    assert!(matches!(
        receive_direct_to_device(&state, "there.example", &no_id)
            .await
            .unwrap(),
        InboundToDevice::Dropped(_)
    ));
    assert!(inbox(&state, &alice_id, &alice_device).await.is_empty());
    // The forged one did not use up the real sender's message_id either.
    assert_eq!(
        receive_direct_to_device(&state, "there.example", &forged)
            .await
            .unwrap(),
        InboundToDevice::Delivered { messages: 1 }
    );
}
