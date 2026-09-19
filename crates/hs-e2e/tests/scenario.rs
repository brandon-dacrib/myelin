//! Scenario tests over real HTTP: two users register, upload device and one-time keys, query
//! each other's keys, and claim one-time keys from each other -- driven through `hs-testkit`'s
//! `Scenario` DSL against a router that mounts both `hs-auth`'s real router (for registration)
//! and this crate's own `hs_e2e::routes::router` fragment, sharing one `AuthState`. Mirrors
//! `crates/hs-room/tests/scenario.rs`'s composition pattern.
//!
//! Request bodies with a dynamic (non-literal) JSON object key are built with [`obj1`]/[`obj2`]
//! rather than `serde_json::json!{ variable: ... }`, since the `json!` macro's object-key
//! position is documented for literal and identifier keys, not general expressions.

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use hs_auth::state::AuthState;
use hs_e2e::state::E2eState;
use hs_e2e::store::tables::TablesE2eStore;
use hs_kv::memory::MemoryBackend;
use hs_testkit::Scenario;
use serde_json::{Value, json};

fn obj1(k: impl Into<String>, v: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(k.into(), v);
    Value::Object(map)
}

fn obj2(k1: impl Into<String>, v1: Value, k2: impl Into<String>, v2: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(k1.into(), v1);
    map.insert(k2.into(), v2);
    Value::Object(map)
}

fn app() -> axum::Router {
    let auth_state = AuthState::in_memory();
    let backend = MemoryBackend::new();
    let store = TablesE2eStore::open(backend).expect("open e2e store");
    let e2e_state: E2eState<MemoryBackend> = E2eState::new(auth_state.clone(), Arc::new(store));
    let (e2e_router, _manifest) = hs_e2e::routes::router::<MemoryBackend>();

    hs_auth::routes::router()
        .with_state(auth_state)
        .merge(e2e_router.with_state(e2e_state))
}

#[tokio::test]
async fn two_users_upload_query_and_claim_keys() {
    let mut scenario = Scenario::new(app());

    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();

    let alice_id = scenario.session("alice").unwrap().user_id.clone().unwrap();
    let alice_device = scenario
        .session("alice")
        .unwrap()
        .device_id
        .clone()
        .unwrap();
    let bob_id = scenario.session("bob").unwrap().user_id.clone().unwrap();
    let bob_device = scenario.session("bob").unwrap().device_id.clone().unwrap();

    // --- alice uploads device keys and one one-time key ---
    let alice_device_keys = json!({
        "user_id": alice_id,
        "device_id": alice_device,
        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
        "keys": obj2(
            format!("curve25519:{alice_device}"), json!("aliceCurveKey"),
            format!("ed25519:{alice_device}"), json!("aliceEd25519Key"),
        ),
        "signatures": {},
    });
    let alice_body = obj2(
        "device_keys",
        alice_device_keys,
        "one_time_keys",
        obj1(
            "signed_curve25519:AAAAAQ",
            json!({"key": "aliceOtk1", "signatures": {}}),
        ),
    );
    let alice_upload = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/upload",
            Some(alice_body),
        )
        .await;
    alice_upload.assert_ok();
    assert_eq!(
        alice_upload.json["one_time_key_counts"]["signed_curve25519"],
        1
    );

    // --- bob uploads device keys only ---
    let bob_device_keys = json!({
        "user_id": bob_id,
        "device_id": bob_device,
        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
        "keys": obj2(
            format!("curve25519:{bob_device}"), json!("bobCurveKey"),
            format!("ed25519:{bob_device}"), json!("bobEd25519Key"),
        ),
        "signatures": {},
    });
    let bob_upload = scenario
        .send(
            Some("bob"),
            Method::POST,
            "/keys/upload",
            Some(obj1("device_keys", bob_device_keys)),
        )
        .await;
    bob_upload.assert_ok();

    // --- bob queries alice's keys ---
    let query_body = obj1("device_keys", obj1(alice_id.clone(), json!([])));
    let query = scenario
        .send(Some("bob"), Method::POST, "/keys/query", Some(query_body))
        .await;
    query.assert_ok();
    let alice_devices = &query.json["device_keys"][&alice_id];
    assert!(
        alice_devices.get(&alice_device).is_some(),
        "bob sees alice's device"
    );

    // --- alice queries bob's keys ---
    let query2_body = obj1("device_keys", obj1(bob_id.clone(), json!([])));
    let query2 = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/query",
            Some(query2_body),
        )
        .await;
    query2.assert_ok();
    assert!(
        query2.json["device_keys"][&bob_id]
            .get(&bob_device)
            .is_some()
    );

    // --- bob claims alice's one-time key ---
    let claim_body = obj1(
        "one_time_keys",
        obj1(
            alice_id.clone(),
            obj1(alice_device.clone(), json!("signed_curve25519")),
        ),
    );
    let claim = scenario
        .send(
            Some("bob"),
            Method::POST,
            "/keys/claim",
            Some(claim_body.clone()),
        )
        .await;
    claim.assert_ok();
    let claimed_map = claim.json["one_time_keys"][&alice_id][&alice_device]
        .as_object()
        .expect("a claimed key object");
    assert_eq!(claimed_map.len(), 1);
    assert!(claimed_map.contains_key("signed_curve25519:AAAAAQ"));

    // --- claiming again: the key is gone, so the response has nothing for alice's device ---
    let claim2 = scenario
        .send(Some("bob"), Method::POST, "/keys/claim", Some(claim_body))
        .await;
    claim2.assert_ok();
    assert!(
        claim2.json["one_time_keys"].get(&alice_id).is_none(),
        "a second claim of the same (and only) one-time key must find nothing left -- this is \
         the double-claim guarantee, exercised over real HTTP"
    );

    // --- and the upload endpoint now reports zero remaining keys for alice ---
    let recount = scenario
        .send(Some("alice"), Method::POST, "/keys/upload", Some(json!({})))
        .await;
    recount.assert_ok();
    assert!(
        recount.json["one_time_key_counts"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn key_backup_version_and_session_round_trip() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("carol", "carol", "correct horse battery staple")
        .await
        .assert_ok();

    let create = scenario
        .send(
            Some("carol"),
            Method::POST,
            "/room_keys/version",
            Some(json!({
                "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
                "auth_data": {"public_key": "abc"},
            })),
        )
        .await;
    create.assert_ok();
    let version = create.str_field("version").to_string();

    let session_body = json!({
        "first_message_index": 0,
        "forwarded_count": 0,
        "is_verified": true,
        "session_data": {"ciphertext": "..."},
    });

    let put = scenario
        .send(
            Some("carol"),
            Method::PUT,
            &format!("/room_keys/keys/%21room%3Aexample.org/session1?version={version}"),
            Some(session_body.clone()),
        )
        .await;
    put.assert_ok();
    assert_eq!(put.json["count"], 1);

    let get = scenario
        .send(
            Some("carol"),
            Method::GET,
            &format!("/room_keys/keys/%21room%3Aexample.org/session1?version={version}"),
            None,
        )
        .await;
    get.assert_status(StatusCode::OK);
    assert_eq!(get.json["session_data"]["ciphertext"], "...");

    // Wrong version is rejected.
    let wrong = scenario
        .send(
            Some("carol"),
            Method::PUT,
            "/room_keys/keys/%21room%3Aexample.org/session2?version=999",
            Some(session_body),
        )
        .await;
    wrong.assert_matrix_error(StatusCode::FORBIDDEN, "M_WRONG_ROOM_KEYS_VERSION");
}

#[tokio::test]
async fn send_to_device_delivers_and_is_idempotent_on_retry() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("dave", "dave", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("erin", "erin", "correct horse battery staple")
        .await
        .assert_ok();
    let erin_id = scenario.session("erin").unwrap().user_id.clone().unwrap();
    let erin_device = scenario.session("erin").unwrap().device_id.clone().unwrap();

    let send_body = obj1(
        "messages",
        obj1(erin_id, obj1(erin_device, json!({"n": 1}))),
    );

    let send = scenario
        .send(
            Some("dave"),
            Method::PUT,
            "/sendToDevice/m.room_key/txn1",
            Some(send_body.clone()),
        )
        .await;
    send.assert_ok();

    // A retry with the same transaction id must not deliver a second copy -- there is no
    // observable state through this crate's HTTP surface alone to prove that (delivery is read
    // by sync, track 05, not yet landed), so this only proves the retry itself succeeds
    // idempotently rather than erroring.
    let retry = scenario
        .send(
            Some("dave"),
            Method::PUT,
            "/sendToDevice/m.room_key/txn1",
            Some(send_body),
        )
        .await;
    retry.assert_ok();
}
