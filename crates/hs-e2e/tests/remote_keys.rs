//! `/keys/query` and `/keys/claim` for users of other servers go to those servers
//! (`hs_e2e::federation::RemoteKeys`), and what comes back is believed only about the users that
//! server owns; the federation-side answers (`federation_keys_query`, `federation_keys_claim`)
//! give this server's own users' keys and nothing private.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::Method;
use hs_auth::state::AuthState;
use hs_e2e::federation::RemoteKeys;
use hs_e2e::state::E2eState;
use hs_e2e::store::tables::TablesE2eStore;
use hs_kv::memory::MemoryBackend;
use hs_testkit::Scenario;
use serde_json::{Value, json};

/// Answers for `there.example` and fails for every other server; records what it was asked.
#[derive(Default)]
struct FakeRemote(Mutex<Vec<(String, String, Value)>>);

#[async_trait]
impl RemoteKeys for FakeRemote {
    async fn query(&self, server: &str, device_keys: Value) -> Result<Value, String> {
        self.0
            .lock()
            .unwrap()
            .push(("query".to_owned(), server.to_owned(), device_keys));
        if server != "there.example" {
            return Err("connection refused".to_owned());
        }
        Ok(json!({
            "device_keys": {
                "@bob:there.example": {"BOBDEV": {"device_id": "BOBDEV", "keys": {"ed25519:BOBDEV": "k"}}},
                "@alice:localhost": {"FORGED": {"device_id": "FORGED"}},
            },
            "master_keys": {"@bob:there.example": {"usage": ["master"]}},
        }))
    }

    async fn claim(&self, server: &str, one_time_keys: Value) -> Result<Value, String> {
        self.0
            .lock()
            .unwrap()
            .push(("claim".to_owned(), server.to_owned(), one_time_keys));
        Ok(json!({"one_time_keys": {
            "@bob:there.example": {"BOBDEV": {"signed_curve25519:AAAA": {"key": "otk"}}}
        }}))
    }
}

fn app(remote: Option<Arc<FakeRemote>>) -> (axum::Router, E2eState<MemoryBackend>) {
    let auth_state = AuthState::in_memory();
    let store = TablesE2eStore::open(MemoryBackend::new()).expect("open e2e store");
    let e2e_state: E2eState<MemoryBackend> = E2eState::new(auth_state.clone(), Arc::new(store));
    if let Some(remote) = remote {
        e2e_state.install_remote_keys(remote);
    }
    let (e2e_router, _manifest) = hs_e2e::routes::router::<MemoryBackend>();
    (
        hs_auth::routes::router()
            .with_state(auth_state)
            .merge(e2e_router.with_state(e2e_state.clone())),
        e2e_state,
    )
}

#[tokio::test]
async fn a_remote_users_keys_are_asked_of_their_server_and_believed_only_about_its_users() {
    let remote = Arc::new(FakeRemote::default());
    let (router, _) = app(Some(remote.clone()));
    let mut scenario = Scenario::new(router);
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let alice_id = scenario.session("alice").unwrap().user_id.clone().unwrap();

    let query = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/query",
            Some(json!({"device_keys": {
                "@bob:there.example": [],
                "@carol:down.example": [],
                alice_id.clone(): [],
            }})),
        )
        .await;
    query.assert_ok();
    let body = &query.json;
    assert_eq!(
        body["device_keys"]["@bob:there.example"]["BOBDEV"]["keys"]["ed25519:BOBDEV"],
        "k"
    );
    assert_eq!(
        body["master_keys"]["@bob:there.example"]["usage"],
        json!(["master"])
    );
    assert!(
        body["device_keys"][&alice_id].get("FORGED").is_none(),
        "a remote server's word about a local user is not taken: {body}"
    );
    assert!(body["failures"].get("down.example").is_some(), "{body}");
    let asked = remote.0.lock().unwrap().clone();
    assert!(asked.iter().all(|(_, server, users)| {
        users
            .as_object()
            .unwrap()
            .keys()
            .all(|u| u.ends_with(&format!(":{server}")))
    }));
    assert_eq!(asked.len(), 2, "one request per remote server: {asked:?}");

    let claim = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/claim",
            Some(json!({"one_time_keys": {"@bob:there.example": {"BOBDEV": "signed_curve25519"}}})),
        )
        .await;
    claim.assert_ok();
    assert_eq!(
        claim.json["one_time_keys"]["@bob:there.example"]["BOBDEV"]["signed_curve25519:AAAA"]["key"],
        "otk"
    );
}

#[tokio::test]
async fn without_remote_access_a_remote_user_is_simply_absent() {
    let (router, _) = app(None);
    let mut scenario = Scenario::new(router);
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let query = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/query",
            Some(json!({"device_keys": {"@bob:there.example": []}})),
        )
        .await;
    query.assert_ok();
    assert!(
        query.json["device_keys"]
            .get("@bob:there.example")
            .is_none()
    );
    assert_eq!(query.json["failures"], json!({}));
}

/// The answer another server gets: this server's users' device and cross-signing keys, never a
/// user-signing key, and nothing about users of any other server.
#[tokio::test]
async fn the_federation_answer_is_local_users_only_and_has_no_user_signing_keys() {
    let (router, state) = app(None);
    let mut scenario = Scenario::new(router);
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
    let session = scenario.session("alice").unwrap().clone();
    let alice_id = session.user_id.clone().unwrap();
    let device_id = session.device_id.clone().unwrap();
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/upload",
            Some(json!({"device_keys": {
                "user_id": alice_id,
                "device_id": device_id,
                "algorithms": ["m.olm.v1.curve25519-aes-sha2"],
                "keys": {format!("ed25519:{device_id}"): "abc"},
                "signatures": {},
            }})),
        )
        .await
        .assert_ok();

    let answer = hs_e2e::federation::federation_keys_query(
        &state,
        &json!({alice_id.clone(): [], "@bob:there.example": []}),
    )
    .await
    .unwrap();
    assert_eq!(
        answer["device_keys"][&alice_id][&device_id]["keys"][format!("ed25519:{device_id}")],
        "abc"
    );
    assert!(answer["device_keys"].get("@bob:there.example").is_none());
    assert!(answer.get("user_signing_keys").is_none());
}
