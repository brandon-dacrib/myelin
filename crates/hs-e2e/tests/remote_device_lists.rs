//! The copy this server keeps of remote users' device lists (`hs_e2e::federation`'s module
//! docs): how it is filled from `/user/devices`, kept current from `m.device_list_update` and
//! `m.signing_key_update` EDUs, served by `/keys/query` without asking anybody, and fetched again
//! when an update skipped something. The scenarios are Sytest's
//! `50federation/40devicelists.pl`, in-process: a fake remote server stands in for Sytest's.
//! Composition mirrors `tests/remote_keys.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::Method;
use hs_auth::state::AuthState;
use hs_e2e::federation::{
    InboundDeviceList, RemoteKeys, RoomSharing, receive_device_list_update,
    receive_signing_key_update,
};
use hs_e2e::state::E2eState;
use hs_e2e::store::tables::TablesE2eStore;
use hs_kv::memory::MemoryBackend;
use hs_testkit::Scenario;
use ruma::UserId;
use serde_json::{Value, json};

const BOB: &str = "@bob:there.example";

/// A remote server whose `/user/devices` answer and reachability the test controls, recording
/// every request made of it.
#[derive(Default)]
struct FakeRemote {
    devices_answer: Mutex<Option<Value>>,
    keys_answer: Mutex<Option<Value>>,
    asked: Mutex<Vec<String>>,
}

impl FakeRemote {
    fn set_devices(&self, answer: Value) {
        *self.devices_answer.lock().unwrap() = Some(answer);
    }
    fn set_keys(&self, answer: Value) {
        *self.keys_answer.lock().unwrap() = Some(answer);
    }
    fn go_down(&self) {
        *self.devices_answer.lock().unwrap() = None;
        *self.keys_answer.lock().unwrap() = None;
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl RemoteKeys for FakeRemote {
    async fn query(&self, server: &str, device_keys: Value) -> Result<Value, String> {
        self.asked.lock().unwrap().push(format!("query {server}"));
        let _ = device_keys;
        self.keys_answer
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "connection refused".to_owned())
    }

    async fn claim(&self, _server: &str, _one_time_keys: Value) -> Result<Value, String> {
        Err("not asked in these tests".to_owned())
    }

    async fn devices(&self, server: &str, user_id: &str) -> Result<Value, String> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("devices {server} {user_id}"));
        self.devices_answer
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "connection refused".to_owned())
    }
}

/// Shares a room with everybody, or with nobody; the test flips it.
struct Sharing(AtomicBool);

#[async_trait]
impl RoomSharing for Sharing {
    async fn shares_a_room_with_a_local_user(&self, _user_id: &UserId) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn app(remote: Arc<FakeRemote>, sharing: bool) -> (axum::Router, E2eState<MemoryBackend>) {
    app_with(remote, Arc::new(Sharing(AtomicBool::new(sharing)))).0
}

fn app_with(
    remote: Arc<FakeRemote>,
    sharing: Arc<Sharing>,
) -> ((axum::Router, E2eState<MemoryBackend>), Arc<Sharing>) {
    let auth_state = AuthState::in_memory();
    let store = TablesE2eStore::open(MemoryBackend::new()).expect("open e2e store");
    let e2e_state: E2eState<MemoryBackend> = E2eState::new(auth_state.clone(), Arc::new(store));
    e2e_state.install_remote_keys(remote);
    e2e_state.install_room_sharing(sharing.clone());
    let (e2e_router, _manifest) = hs_e2e::routes::router::<MemoryBackend>();
    (
        (
            hs_auth::routes::router()
                .with_state(auth_state)
                .merge(e2e_router.with_state(e2e_state.clone())),
            e2e_state,
        ),
        sharing,
    )
}

fn bobs_list(stream_id: u64, devices: Vec<Value>) -> Value {
    json!({"user_id": BOB, "stream_id": stream_id, "devices": devices})
}

fn device(id: &str, key: &str, name: Option<&str>) -> Value {
    let mut d =
        json!({"device_id": id, "keys": {"device_id": id, "keys": {format!("ed25519:{id}"): key}}});
    if let Some(name) = name {
        d["device_display_name"] = Value::String(name.to_owned());
    }
    d
}

async fn query_bob(scenario: &mut Scenario) -> Value {
    let response = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/query",
            Some(json!({"device_keys": {BOB: []}})),
        )
        .await;
    response.assert_ok();
    response.json
}

async fn register_alice(scenario: &mut Scenario) {
    scenario
        .register("alice", "alice", "correct horse battery staple")
        .await
        .assert_ok();
}

/// Sytest's "Server correctly resyncs when client query keys and there is no remote cache" and
/// "Device list doesn't change if remote server is down": the first query of a user who shares
/// a room fetches their whole list (`GET /user/devices`, not `/user/keys/query`) and keeps it;
/// the next is answered from the copy with no request at all, so it still answers, with no
/// `failures`, when their server cannot be reached.
#[tokio::test]
async fn a_shared_users_list_is_fetched_once_and_then_served_from_the_copy_even_when_their_server_is_down()
 {
    let remote = Arc::new(FakeRemote::default());
    remote.set_devices(bobs_list(
        3,
        vec![
            device("ROVER", "k1", Some("Curiosity Rover")),
            device("LANDER", "k2", None),
        ],
    ));
    let (router, _) = app(remote.clone(), true);
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;

    let first = query_bob(&mut scenario).await;
    assert_eq!(
        first["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "k1"
    );
    assert_eq!(
        first["device_keys"][BOB]["ROVER"]["unsigned"]["device_display_name"],
        "Curiosity Rover"
    );
    assert_eq!(
        first["device_keys"][BOB]["LANDER"]["keys"]["ed25519:LANDER"],
        "k2"
    );
    assert_eq!(first["failures"], json!({}));
    assert_eq!(remote.asked(), [format!("devices there.example {BOB}")]);

    remote.go_down();
    let second = query_bob(&mut scenario).await;
    assert_eq!(second["device_keys"], first["device_keys"]);
    assert_eq!(second["failures"], json!({}));
    assert_eq!(
        remote.asked().len(),
        1,
        "nothing was asked: {:?}",
        remote.asked()
    );
}

/// Complement's `TestDeviceListUpdates` ("must not return a cached device list"): once a user
/// shares no room here any more, no update is coming for them, so the copy is dropped at the
/// next query and their server asked instead -- and a copy is kept again once they do.
#[tokio::test]
async fn the_copy_is_dropped_once_the_user_shares_no_room_any_more() {
    let remote = Arc::new(FakeRemote::default());
    remote.set_devices(bobs_list(1, vec![device("ROVER", "old", None)]));
    remote.set_keys(json!({"device_keys": {BOB: {"ROVER": {"keys": {"ed25519:ROVER": "new"}}}}}));
    let ((router, state), sharing) =
        app_with(remote.clone(), Arc::new(Sharing(AtomicBool::new(true))));
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;
    let first = query_bob(&mut scenario).await;
    assert_eq!(
        first["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "old"
    );

    // Bob leaves the one room he shared and changes his keys; nobody told this server.
    sharing.0.store(false, Ordering::SeqCst);
    let after_leaving = query_bob(&mut scenario).await;
    assert_eq!(
        after_leaving["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "new"
    );
    assert_eq!(
        remote.asked(),
        [
            format!("devices there.example {BOB}"),
            "query there.example".to_owned()
        ]
    );
    assert!(
        state
            .store
            .get_remote_user(&UserId::parse(BOB).unwrap())
            .await
            .unwrap()
            .is_none()
    );

    // He is back: the list is fetched and kept again.
    sharing.0.store(true, Ordering::SeqCst);
    remote.set_devices(bobs_list(2, vec![device("ROVER", "new", None)]));
    let rejoined = query_bob(&mut scenario).await;
    assert_eq!(
        rejoined["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "new"
    );
    assert_eq!(remote.asked().len(), 3);
    query_bob(&mut scenario).await;
    assert_eq!(remote.asked().len(), 3, "served from the copy again");
}

/// A user who shares no room with anyone here is not copied: their server is asked
/// `/user/keys/query` every time, as before the copy existed.
#[tokio::test]
async fn a_user_sharing_no_room_is_asked_of_their_server_every_time_and_not_kept() {
    let remote = Arc::new(FakeRemote::default());
    remote.set_keys(json!({"device_keys": {BOB: {"ROVER": {"keys": {"ed25519:ROVER": "k1"}}}}}));
    let (router, state) = app(remote.clone(), false);
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;

    let answer = query_bob(&mut scenario).await;
    assert_eq!(
        answer["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "k1"
    );
    query_bob(&mut scenario).await;
    assert_eq!(
        remote.asked(),
        ["query there.example", "query there.example"]
    );
    assert!(
        state
            .store
            .get_remote_user(&UserId::parse(BOB).unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

/// Sytest's "Server correctly handles incoming m.device_list_update": an update for a user
/// whose list is not held fetches the whole list; one that follows the copy (`prev_id` is the
/// copy's position) is applied to it directly, display name included; both record a change.
#[tokio::test]
async fn an_update_for_an_unknown_user_fetches_the_list_and_one_in_sequence_is_applied() {
    let remote = Arc::new(FakeRemote::default());
    remote.set_devices(bobs_list(1, vec![device("ROVER", "k1", None)]));
    let (router, state) = app(remote.clone(), true);
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;
    let before = state.store.current_stream_pos().await.unwrap();

    let first = receive_device_list_update(
        &state,
        "there.example",
        &json!({"user_id": BOB, "device_id": "ROVER", "stream_id": 1, "keys": {"keys": {"ed25519:ROVER": "k1"}}}),
    )
    .await
    .unwrap();
    assert_eq!(first, InboundDeviceList::Resynced);
    assert_eq!(remote.asked(), [format!("devices there.example {BOB}")]);
    assert!(state.store.current_stream_pos().await.unwrap() > before);

    let second = receive_device_list_update(
        &state,
        "there.example",
        &json!({
            "user_id": BOB, "device_id": "ROVER", "stream_id": 2, "prev_id": [1],
            "device_display_name": "test display name",
            "keys": {"keys": {"ed25519:ROVER": "k1"}},
        }),
    )
    .await
    .unwrap();
    assert_eq!(second, InboundDeviceList::Applied);
    assert_eq!(remote.asked().len(), 1, "applied without fetching");

    let answer = query_bob(&mut scenario).await;
    assert_eq!(
        answer["device_keys"][BOB]["ROVER"]["unsigned"]["device_display_name"],
        "test display name"
    );
    assert_eq!(remote.asked().len(), 1, "served from the copy");
}

/// A client signs in (a device with no keys yet) and uploads its keys a moment later, and the
/// user's server announces that as two updates when its announcer looks between them. The
/// first adds the device to the copy without keys, so `/keys/query` names nothing for it yet:
/// the copy is complete and current, and nothing is fetched or asked; the second brings the
/// keys, and the next query serves them, still from the copy. (The two-server test in `hs-cli`,
/// `federation_edus::a_device_added_on_one_server_is_a_device_list_change_on_the_other`, failed
/// on a slow CI machine by querying between the two; it waits for the second now.)
#[tokio::test]
async fn a_device_added_without_keys_is_keyed_by_the_next_update_and_nothing_is_fetched_between() {
    let remote = Arc::new(FakeRemote::default());
    remote.set_devices(bobs_list(1, vec![device("ROVER", "k1", None)]));
    let (router, state) = app(remote.clone(), true);
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;
    assert_eq!(
        query_bob(&mut scenario).await["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "k1"
    );
    assert_eq!(remote.asked().len(), 1, "fetched once");

    // The sign-in: a device with no keys, announced on its own.
    let login = receive_device_list_update(
        &state,
        "there.example",
        &json!({"user_id": BOB, "device_id": "LAPTOP", "stream_id": 2, "prev_id": [1]}),
    )
    .await
    .unwrap();
    assert_eq!(login, InboundDeviceList::Applied);
    let between = query_bob(&mut scenario).await;
    assert_eq!(
        between["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "k1"
    );
    assert_eq!(
        between["device_keys"][BOB]["LAPTOP"],
        Value::Null,
        "a device without keys is not in the answer: {between}"
    );
    assert_eq!(between["failures"], json!({}));
    assert_eq!(
        remote.asked().len(),
        1,
        "the copy is current; nothing fetched or asked"
    );

    // The key upload, in sequence.
    let keyed = receive_device_list_update(
        &state,
        "there.example",
        &json!({
            "user_id": BOB, "device_id": "LAPTOP", "stream_id": 3, "prev_id": [2],
            "keys": {"keys": {"ed25519:LAPTOP": "k2"}},
        }),
    )
    .await
    .unwrap();
    assert_eq!(keyed, InboundDeviceList::Applied);
    let after = query_bob(&mut scenario).await;
    assert_eq!(
        after["device_keys"][BOB]["LAPTOP"]["keys"]["ed25519:LAPTOP"],
        "k2"
    );
    assert_eq!(
        after["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "k1"
    );
    assert_eq!(remote.asked().len(), 1, "served from the copy");
}

/// Sytest's "If a device list update goes missing, the server resyncs on the next one": an
/// update whose `prev_id` is past the copy's position means one was missed, so the whole list
/// is fetched again and the keys the missed update carried are known afterwards.
#[tokio::test]
async fn an_update_that_skipped_one_fetches_the_whole_list_again() {
    let remote = Arc::new(FakeRemote::default());
    remote.set_devices(bobs_list(
        1,
        vec![json!({"device_id": "ROVER", "keys": {"keys": {}}, "device_display_name": "Original name"})],
    ));
    let (router, state) = app(remote.clone(), true);
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;
    receive_device_list_update(
        &state,
        "there.example",
        &json!({"user_id": BOB, "device_id": "ROVER", "stream_id": 1}),
    )
    .await
    .unwrap();

    // Stream 2 (the keys) is never sent; stream 3 arrives naming it.
    remote.set_devices(bobs_list(
        3,
        vec![device("ROVER", "LOu9tc6Sg7", Some("New device name"))],
    ));
    let third = receive_device_list_update(
        &state,
        "there.example",
        &json!({"user_id": BOB, "device_id": "ROVER", "stream_id": 3, "prev_id": [2], "device_display_name": "New device name"}),
    )
    .await
    .unwrap();
    assert_eq!(third, InboundDeviceList::Resynced);
    assert_eq!(remote.asked().len(), 2);

    let answer = query_bob(&mut scenario).await;
    assert_eq!(
        answer["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "LOu9tc6Sg7"
    );
    assert_eq!(
        answer["device_keys"][BOB]["ROVER"]["unsigned"]["device_display_name"],
        "New device name"
    );
}

/// When the fetch an out-of-sequence update calls for fails, the copy is stale: it is not
/// served (the user's server is asked, and its being down is reported in `failures`) until a
/// later query fetches it again.
#[tokio::test]
async fn a_copy_that_could_not_be_fetched_again_is_stale_until_it_is() {
    let remote = Arc::new(FakeRemote::default());
    remote.set_devices(bobs_list(1, vec![device("ROVER", "k1", None)]));
    let (router, state) = app(remote.clone(), true);
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;
    query_bob(&mut scenario).await;

    remote.go_down();
    let outcome = receive_device_list_update(
        &state,
        "there.example",
        &json!({"user_id": BOB, "device_id": "ROVER", "stream_id": 5, "prev_id": [4]}),
    )
    .await
    .unwrap();
    assert_eq!(outcome, InboundDeviceList::ResyncFailed);
    let while_down = query_bob(&mut scenario).await;
    assert!(
        while_down["failures"].get("there.example").is_some(),
        "{while_down}"
    );
    assert!(while_down["device_keys"].get(BOB).is_none(), "{while_down}");

    remote.set_devices(bobs_list(5, vec![device("ROVER", "k5", None)]));
    let after = query_bob(&mut scenario).await;
    assert_eq!(
        after["device_keys"][BOB]["ROVER"]["keys"]["ed25519:ROVER"],
        "k5"
    );
    assert_eq!(after["failures"], json!({}));
}

/// A deletion in sequence removes the device from the copy; an update at or before the copy's
/// position is already known and changes nothing.
#[tokio::test]
async fn a_deletion_in_sequence_removes_the_device_and_an_old_update_is_ignored() {
    let remote = Arc::new(FakeRemote::default());
    remote.set_devices(bobs_list(
        2,
        vec![device("ROVER", "k1", None), device("LANDER", "k2", None)],
    ));
    let (router, state) = app(remote.clone(), true);
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;
    query_bob(&mut scenario).await;

    let old = receive_device_list_update(
        &state,
        "there.example",
        &json!({"user_id": BOB, "device_id": "ROVER", "stream_id": 2, "prev_id": [1], "deleted": true}),
    )
    .await
    .unwrap();
    assert_eq!(old, InboundDeviceList::AlreadyKnown);

    let deletion = receive_device_list_update(
        &state,
        "there.example",
        &json!({"user_id": BOB, "device_id": "LANDER", "stream_id": 3, "prev_id": [2], "deleted": true}),
    )
    .await
    .unwrap();
    assert_eq!(deletion, InboundDeviceList::Applied);
    let answer = query_bob(&mut scenario).await;
    assert!(
        answer["device_keys"][BOB].get("LANDER").is_none(),
        "{answer}"
    );
    assert!(
        answer["device_keys"][BOB].get("ROVER").is_some(),
        "{answer}"
    );
    assert_eq!(remote.asked().len(), 1);
}

/// An `m.signing_key_update` replaces the cross-signing keys in the copy, so the next query
/// carries the new master key without a fetch; an EDU about a user of another server than its
/// origin is dropped.
#[tokio::test]
async fn a_signing_key_update_changes_the_copied_master_key_and_a_forged_one_is_dropped() {
    let remote = Arc::new(FakeRemote::default());
    let mut list = bobs_list(1, vec![device("ROVER", "k1", None)]);
    list["master_key"] = json!({"user_id": BOB, "usage": ["master"], "keys": {"ed25519:m1": "m1"}});
    remote.set_devices(list);
    let (router, state) = app(remote.clone(), true);
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;
    let first = query_bob(&mut scenario).await;
    assert_eq!(first["master_keys"][BOB]["keys"]["ed25519:m1"], "m1");

    let applied = receive_signing_key_update(
        &state,
        "there.example",
        &json!({"user_id": BOB, "master_key": {"user_id": BOB, "usage": ["master"], "keys": {"ed25519:m2": "m2"}}, "self_signing_key": {"usage": ["self_signing"], "keys": {"ed25519:s2": "s2"}}}),
    )
    .await
    .unwrap();
    assert_eq!(applied, InboundDeviceList::Applied);
    let second = query_bob(&mut scenario).await;
    assert_eq!(second["master_keys"][BOB]["keys"]["ed25519:m2"], "m2");
    assert_eq!(second["self_signing_keys"][BOB]["keys"]["ed25519:s2"], "s2");
    assert_eq!(remote.asked().len(), 1);

    let forged = receive_device_list_update(
        &state,
        "elsewhere.example",
        &json!({"user_id": BOB, "device_id": "EVIL", "stream_id": 9}),
    )
    .await
    .unwrap();
    assert!(
        matches!(forged, InboundDeviceList::Dropped(_)),
        "{forged:?}"
    );
}

/// The other direction: what this server tells another about one of its own users -- every
/// device `hs-auth` knows, keys when uploaded, the display name, the user's own stream position
/// and their cross-signing keys; and `/keys/query` for a local user names the display name
/// under `unsigned` (Sytest's "Can query device keys using POST" wants `unsigned` present and
/// no name when none is set).
#[tokio::test]
async fn a_local_users_list_for_other_servers_has_every_device_named_and_keyed() {
    let remote = Arc::new(FakeRemote::default());
    let (router, state) = app(remote, true);
    let mut scenario = Scenario::new(router);
    register_alice(&mut scenario).await;
    let session = scenario.session("alice").unwrap().clone();
    let alice_id = session.user_id.clone().unwrap();
    let device_id = session.device_id.clone().unwrap();
    scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/upload",
            Some(json!({"device_keys": {
                "user_id": alice_id, "device_id": device_id,
                "algorithms": ["m.olm.v1.curve25519-aes-sha2"],
                "keys": {format!("ed25519:{device_id}"): "abc"}, "signatures": {},
            }})),
        )
        .await
        .assert_ok();
    let own = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/query",
            Some(json!({"device_keys": {alice_id.clone(): []}})),
        )
        .await;
    own.assert_ok();
    assert_eq!(
        own.json["device_keys"][&alice_id][&device_id]["unsigned"],
        json!({})
    );

    scenario
        .send(
            Some("alice"),
            Method::PUT,
            &format!("/devices/{device_id}"),
            Some(json!({"display_name": "wibble"})),
        )
        .await
        .assert_ok();
    let own = scenario
        .send(
            Some("alice"),
            Method::POST,
            "/keys/query",
            Some(json!({"device_keys": {alice_id.clone(): []}})),
        )
        .await;
    assert_eq!(
        own.json["device_keys"][&alice_id][&device_id]["unsigned"]["device_display_name"],
        "wibble"
    );

    let alice = UserId::parse(&alice_id).unwrap();
    let answer = hs_e2e::federation::federation_user_devices(&state, &alice)
        .await
        .unwrap()
        .expect("a local user");
    assert_eq!(answer["user_id"], alice_id);
    assert_eq!(
        answer["stream_id"],
        state.store.user_stream_pos(&alice).await.unwrap()
    );
    let devices = answer["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1, "{answer}");
    assert_eq!(devices[0]["device_id"], device_id);
    assert_eq!(devices[0]["device_display_name"], "wibble");
    assert_eq!(
        devices[0]["keys"]["keys"][format!("ed25519:{device_id}")],
        "abc"
    );
    assert!(
        hs_e2e::federation::federation_user_devices(&state, &UserId::parse(BOB).unwrap())
            .await
            .unwrap()
            .is_none(),
        "not this server's user"
    );
}
