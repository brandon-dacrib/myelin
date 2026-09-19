//! Regression tests for three bugs Complement (`refs/complement/tests/csapi/upload_keys_test.go`,
//! `user_query_keys_test.go`) found in this crate's routes, run for the first time against a real
//! `hs serve` process in this session's own report (`docs/status/14-test-and-conformance.md`). Each
//! test here fails against the pre-fix code and is the router-level, no-Docker-needed proof that
//! stands in for (and is much faster than) rerunning the full Complement suite on every change.
//!
//! Composition mirrors `tests/scenario.rs`: `hs-auth`'s real router for registration, merged with
//! this crate's own router, driven through `hs-testkit`'s `Scenario` DSL.

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

/// Bug 1: `POST /keys/query` for a *different* user than the caller must report that user's
/// entry, even when there is nothing to report -- omitting the key entirely (rather than
/// returning `{}`) is exactly what Complement's `upload_keys_test.go` "query for user with no
/// keys returns empty key dict" catches (`match.JSONKeyTypeEqual` requires the field to exist).
/// This is the same code path a query for a user *with* devices uses (`build_keys_query_response`
/// inserted a per-user entry only `if !per_user.is_empty()`), so proving the empty case is fixed
/// is the general fix, not a special case for it.
#[tokio::test]
async fn keys_query_reports_an_entry_for_another_user_with_no_devices() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();
    let bob_id = scenario.session("bob").unwrap().user_id.clone().unwrap();

    // alice queries bob -- a genuinely different user -- who has never uploaded any device keys.
    let query_body = obj1("device_keys", obj1(bob_id.clone(), json!([])));
    let query = scenario
        .send(Some("alice"), Method::POST, "/keys/query", Some(query_body))
        .await;
    query.assert_ok();
    let device_keys = query
        .json
        .get("device_keys")
        .expect("device_keys must be present at all");
    assert!(
        device_keys.get(&bob_id).is_some(),
        "device_keys.{bob_id} must be present (as an empty object) for a queried user with no \
         devices, not omitted entirely -- got {device_keys:?}"
    );
    assert!(
        device_keys[&bob_id].as_object().unwrap().is_empty(),
        "bob has uploaded no devices, so his entry must be an empty object"
    );
}

/// The same bug, proven with a user who *does* have devices, to rule out any doubt that the fix
/// only touches the empty case: a cross-user query (alice querying bob, never bob querying
/// himself) must return bob's real device under `device_keys.<bob>`.
#[tokio::test]
async fn keys_query_returns_another_users_real_device_in_a_cross_user_query() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();
    let bob_id = scenario.session("bob").unwrap().user_id.clone().unwrap();
    let bob_device = scenario.session("bob").unwrap().device_id.clone().unwrap();

    let bob_device_keys = json!({
        "user_id": bob_id,
        "device_id": bob_device,
        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
        "keys": {
            format!("curve25519:{bob_device}"): "bobCurveKey",
            format!("ed25519:{bob_device}"): "bobEd25519Key",
        },
        "signatures": {},
    });
    scenario
        .send(
            Some("bob"),
            Method::POST,
            "/keys/upload",
            Some(obj1("device_keys", bob_device_keys)),
        )
        .await
        .assert_ok();

    // alice -- not bob -- performs the query.
    let query_body = obj1("device_keys", obj1(bob_id.clone(), json!([])));
    let query = scenario
        .send(Some("alice"), Method::POST, "/keys/query", Some(query_body))
        .await;
    query.assert_ok();
    assert!(
        query.json["device_keys"][&bob_id]
            .get(&bob_device)
            .is_some(),
        "alice must see bob's device in a cross-user query: {:?}",
        query.json
    );
}

/// Bug 3 (query side): a per-user device filter that is an object instead of an array (Element
/// iOS's historical bug, `{"device_id1": true}` in place of `["device_id1"]`) must be rejected
/// with `400 M_BAD_JSON`, not silently treated as "no filter" -- matches Complement's
/// `user_query_keys_test.go::TestKeysQueryWithDeviceIDAsObjectFails`.
#[tokio::test]
async fn keys_query_rejects_device_id_as_object_shape() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();
    let bob_id = scenario.session("bob").unwrap().user_id.clone().unwrap();

    let query_body = obj1(
        "device_keys",
        obj1(bob_id, json!({"device_id1": true, "device_id2": true})),
    );
    let query = scenario
        .send(Some("alice"), Method::POST, "/keys/query", Some(query_body))
        .await;
    query.assert_matrix_error(StatusCode::BAD_REQUEST, "M_BAD_JSON");
}

/// Bug 3 (upload side): `device_keys` missing required fields (`algorithms`/`keys`/`signatures`)
/// must be rejected with `400 M_BAD_JSON` -- matches Complement's `upload_keys_test.go`'s
/// "Rejects invalid device keys".
#[tokio::test]
async fn keys_upload_rejects_device_keys_missing_required_fields() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("bob", "bob", "hunter2official")
        .await
        .assert_ok();
    let bob_id = scenario.session("bob").unwrap().user_id.clone().unwrap();
    let bob_device = scenario.session("bob").unwrap().device_id.clone().unwrap();

    let malformed = json!({
        "device_keys": {
            "user_id": bob_id,
            "device_id": bob_device,
        },
    });
    let upload = scenario
        .send(Some("bob"), Method::POST, "/keys/upload", Some(malformed))
        .await;
    upload.assert_matrix_error(StatusCode::BAD_REQUEST, "M_BAD_JSON");
}

/// Bug 2: `POST /keys/claim` must return the *exact* one-time key object a device uploaded --
/// deep-equal, including a nested `signatures` object -- and, per MSC4225, must claim keys in
/// upload order rather than the lexicographic order of their key ids. This test uploads two keys
/// whose ids sort the *opposite* way from their upload order (`"9"` uploaded first, then `"10"`,
/// which sorts before `"9"` lexicographically) so a lexicographic-first claim strategy would both
/// return the wrong key first *and*, since the two keys carry different `signatures`, return a
/// signature object that does not match what a client claiming "the next key" expects.
#[tokio::test]
async fn keys_claim_returns_the_exact_uploaded_key_in_upload_order() {
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

    let key_9 = json!({
        "key": "firstUploadedKey",
        "signatures": {alice_id.clone(): {format!("ed25519:{alice_device}"): "sigForKey9"}},
    });
    let key_10 = json!({
        "key": "secondUploadedKey",
        "signatures": {alice_id.clone(): {format!("ed25519:{alice_device}"): "sigForKey10"}},
    });

    // Upload "9" first, "10" second -- "10" sorts *before* "9" lexicographically, so a correct,
    // FIFO-ordered implementation must still hand out "9" (uploaded first) on the first claim.
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/upload",
            Some(json!({"one_time_keys": {"signed_curve25519:9": key_9}})),
        )
        .await
        .assert_ok();
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/upload",
            Some(json!({"one_time_keys": {"signed_curve25519:10": key_10}})),
        )
        .await
        .assert_ok();

    let claim_body = obj1(
        "one_time_keys",
        obj1(
            alice_id.clone(),
            obj1(alice_device.clone(), json!("signed_curve25519")),
        ),
    );

    let first_claim = scenario
        .send(
            Some("bob"),
            Method::POST,
            "/keys/claim",
            Some(claim_body.clone()),
        )
        .await;
    first_claim.assert_ok();
    let first_map = first_claim.json["one_time_keys"][&alice_id][&alice_device]
        .as_object()
        .expect("a claimed key object");
    assert_eq!(first_map.len(), 1, "exactly one key claimed: {first_map:?}");
    assert_eq!(
        first_map.get("signed_curve25519:9"),
        Some(&key_9),
        "the first claim must return exactly the first-uploaded key, byte-for-byte including \
         its signatures object -- got {first_map:?}"
    );

    let second_claim = scenario
        .send(Some("bob"), Method::POST, "/keys/claim", Some(claim_body))
        .await;
    second_claim.assert_ok();
    let second_map = second_claim.json["one_time_keys"][&alice_id][&alice_device]
        .as_object()
        .expect("a claimed key object");
    assert_eq!(
        second_map.get("signed_curve25519:10"),
        Some(&key_10),
        "the second claim must return exactly the second-uploaded key, byte-for-byte including \
         its signatures object -- got {second_map:?}"
    );
}

/// Not a bug, but this session's brief asked for the same "does a response drop or reshape
/// something a client uploaded" check on the routes Complement's `csapi` suite does not reach.
/// `POST /keys/device_signing/upload` (store) followed by `POST /keys/signatures/upload` (merge a
/// signature onto an existing device's `device_keys`) followed by `POST /keys/query` (read it
/// back) is the one path where uploaded content flows through both cross-signing endpoints and
/// back out to a client; this proves the device's original `algorithms`/`keys` survive the
/// signature merge untouched and the new signature is present, byte-for-byte.
#[tokio::test]
async fn signatures_upload_merges_without_dropping_the_device_keys_it_signs() {
    let mut scenario = Scenario::new(app());
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let alice_id = scenario.session("alice").unwrap().user_id.clone().unwrap();
    let alice_device = scenario
        .session("alice")
        .unwrap()
        .device_id
        .clone()
        .unwrap();

    let original_algorithms = json!(["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"]);
    let original_keys = json!({
        format!("curve25519:{alice_device}"): "aliceCurveKey",
        format!("ed25519:{alice_device}"): "aliceEd25519Key",
    });
    let device_keys = json!({
        "user_id": alice_id,
        "device_id": alice_device,
        "algorithms": original_algorithms,
        "keys": original_keys,
        "signatures": {},
    });
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/upload",
            Some(obj1("device_keys", device_keys)),
        )
        .await
        .assert_ok();

    // Sign alice's own device with what looks like a self-signing-key signature.
    let signature_upload = obj1(
        alice_id.clone(),
        obj1(
            alice_device.clone(),
            json!({
                "signatures": {
                    alice_id.clone(): {"ed25519:SSK": "aFakeButWellFormedSignature"},
                },
            }),
        ),
    );
    let signed = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/signatures/upload",
            Some(signature_upload),
        )
        .await;
    signed.assert_ok();
    assert!(
        signed.json["failures"].as_object().unwrap().is_empty(),
        "the device exists, so the signature merge must not be reported as a failure: {:?}",
        signed.json
    );

    // Read it back exactly as a client would, via /keys/query.
    let query = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/query",
            Some(obj1("device_keys", obj1(alice_id.clone(), json!([])))),
        )
        .await;
    query.assert_ok();
    let device = &query.json["device_keys"][&alice_id][&alice_device];
    assert_eq!(
        device["algorithms"], original_algorithms,
        "the signature merge must not touch the device's original algorithms"
    );
    assert_eq!(
        device["keys"], original_keys,
        "the signature merge must not touch the device's original keys"
    );
    assert_eq!(
        device["signatures"][&alice_id]["ed25519:SSK"], "aFakeButWellFormedSignature",
        "the newly-merged signature must be readable back exactly as submitted: {device:?}"
    );
}
