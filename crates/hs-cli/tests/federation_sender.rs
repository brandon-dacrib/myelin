//! The outbound half of federation, end to end inside one process: a real `RoomRegistry` (the
//! one `hs serve` uses), the real feeder (`hs_cli::federation_sender`), the real sender
//! (`hs_federation::sender`) and the real outbound client, signing real `X-Matrix` requests to a
//! `FakeFederationPeer` on loopback -- plaintext, via `ClientConfig::scheme`, exactly as
//! `crates/hs-federation/src/client.rs`'s own tests do, because an in-process `hs serve` cannot
//! be told to federate without TLS.
//!
//! What this proves: an event a local user sends in a room with a remote member reaches that
//! member's server as a `PUT /_matrix/federation/v1/send/{txnId}` transaction carrying the event
//! exactly as stored and signed; events from before the remote member joined are not sent; a
//! remote user's own event (their join, signed locally here for the test) is never re-sent; and a
//! kick still reaches the server of the user it removed.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hs_cli::federation_sender::{OutboundFederation, forward_update};
use hs_federation::client::{ClientConfig, FederationClient};
use hs_federation::destination_store::InMemoryDestinationStore;
use hs_federation::discovery::{AddrResolver, SrvResolver, WellKnownFetcher, WellKnownOutcome};
use hs_federation::sender::{FederationSender, SenderConfig};
use hs_kv::memory::MemoryBackend;
use hs_room::actor::CreateRoomRequest;
use hs_room::identity::HomeserverIdentity;
use hs_room::membership::Action;
use hs_room::protocol::RoomUpdate;
use hs_room::registry::RoomRegistry;
use hs_testkit::fake_federation::{FakeFederationPeer, RecordedRequest};
use ruma::OwnedUserId;
use tokio::net::TcpListener;

const US: &str = "local.example";

struct NoWellKnown;
#[async_trait]
impl WellKnownFetcher for NoWellKnown {
    async fn fetch(&self, _hostname: &str) -> WellKnownOutcome {
        WellKnownOutcome::Absent {
            cache_for: Duration::from_secs(60),
        }
    }
}
struct NoSrv;
#[async_trait]
impl SrvResolver for NoSrv {
    async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
        Vec::new()
    }
}
struct FixedAddr(IpAddr);
#[async_trait]
impl AddrResolver for FixedAddr {
    async fn resolve_addr(&self, _hostname: &str) -> Vec<IpAddr> {
        vec![self.0]
    }
}

/// Polls `condition` until it holds or `deadline` passes.
async fn wait_for(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !condition() {
        if start.elapsed() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}

struct Harness {
    peer: FakeFederationPeer,
    /// The remote server's name: `localhost:{port}`, the explicit-port form that bypasses
    /// discovery and is what every user of that server carries as their domain.
    remote: String,
    identity: HomeserverIdentity,
    rooms: Arc<RoomRegistry<MemoryBackend>>,
    sender: Arc<FederationSender>,
}

impl Harness {
    async fn new() -> Self {
        let peer = FakeFederationPeer::new("remote");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = peer.router();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let identity = HomeserverIdentity::for_tests(US);
        let rooms =
            Arc::new(RoomRegistry::open(MemoryBackend::new(), identity.clone()).expect("registry"));
        let client = Arc::new(FederationClient::new(
            US,
            (*identity.signing_key).clone(),
            ClientConfig {
                scheme: "http",
                ..ClientConfig::default()
            },
            Arc::new(InMemoryDestinationStore::new()),
            Arc::new(NoWellKnown),
            Arc::new(NoSrv),
            Arc::new(FixedAddr(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        ));
        let sender = Arc::new(FederationSender::with_config(
            client,
            US,
            SenderConfig {
                initial_backoff: Duration::from_millis(100),
                max_backoff: Duration::from_secs(1),
            },
        ));
        Self {
            peer,
            remote: format!("localhost:{port}"),
            identity,
            rooms,
            sender,
        }
    }

    fn alice() -> OwnedUserId {
        ruma::UserId::parse(format!("@alice:{US}")).unwrap()
    }

    fn bob(&self) -> OwnedUserId {
        ruma::UserId::parse(format!("@bob:{}", self.remote)).unwrap()
    }

    async fn public_room(&self) -> hs_room::actor::RoomActorHandle<MemoryBackend> {
        self.rooms
            .create_room(
                Self::alice(),
                CreateRoomRequest {
                    preset: Some("public_chat".to_owned()),
                    ..Default::default()
                },
                1_000,
            )
            .await
            .expect("room creation")
    }

    /// Waits until the peer has seen at least `n` transactions and nothing is still queued.
    async fn settled_transactions(&self, n: usize) -> Vec<RecordedRequest> {
        assert!(
            wait_for(Duration::from_secs(10), || self.peer.request_count() >= n).await,
            "the remote server received {} transaction(s), expected at least {n}",
            self.peer.request_count()
        );
        assert!(
            wait_for(Duration::from_secs(10), || self.sender.pending_pdus() == 0).await,
            "PDUs still pending: {}",
            self.sender.pending_pdus()
        );
        self.peer.requests()
    }
}

fn pdus_of(request: &RecordedRequest) -> &[serde_json::Value] {
    request.body["pdus"].as_array().map(Vec::as_slice).unwrap()
}

/// Receives updates until the one for `event_id` arrives.
async fn update_for(
    updates: &mut tokio::sync::broadcast::Receiver<RoomUpdate>,
    event_id: &ruma::EventId,
) -> RoomUpdate {
    loop {
        let update = tokio::time::timeout(Duration::from_secs(5), updates.recv())
            .await
            .expect("an update should arrive")
            .expect("the stream should stay open");
        if update.event_id == event_id {
            return update;
        }
    }
}

#[tokio::test]
async fn a_local_message_reaches_the_server_of_a_remote_member_and_nothing_earlier_does() {
    let h = Harness::new().await;
    let outbound = OutboundFederation::start(
        h.rooms.clone(),
        h.sender.clone(),
        h.identity.server_name.clone(),
    );
    let alice = Harness::alice();
    let bob = h.bob();

    let room = h.public_room().await;
    room.send_event(
        alice.clone(),
        "m.room.message".to_owned(),
        None,
        serde_json::json!({ "msgtype": "m.text", "body": "before bob" }),
        None,
        2_000,
    )
    .await
    .expect("message before bob");
    // Bob's join: a remote user's event (signed by this server only because this test has no
    // second server), which this server must never distribute -- his own server does that.
    room.membership(
        bob.clone(),
        Action::Join,
        bob.clone(),
        serde_json::json!({}),
        3_000,
    )
    .await
    .expect("bob joins");
    let message = room
        .send_event(
            alice.clone(),
            "m.room.message".to_owned(),
            None,
            serde_json::json!({ "msgtype": "m.text", "body": "hello bob" }),
            None,
            4_000,
        )
        .await
        .expect("message to bob");

    let requests = h.settled_transactions(1).await;
    assert_eq!(requests.len(), 1, "{requests:?}");
    let txn = &requests[0];
    assert_eq!(txn.method, "PUT");
    assert!(
        txn.path.starts_with("/_matrix/federation/v1/send/"),
        "{}",
        txn.path
    );
    assert_eq!(txn.body["origin"], US);
    assert_eq!(txn.body["edus"], serde_json::json!([]));
    let pdus = pdus_of(txn);
    assert_eq!(
        pdus.len(),
        1,
        "only the event sent while bob was a member: {pdus:?}"
    );
    let pdu = &pdus[0];
    assert_eq!(pdu["content"]["body"], "hello bob");
    assert_eq!(pdu["sender"], alice.as_str());
    assert_eq!(
        pdu["room_id"],
        room.query(|a| a.room_id().to_string()).await
    );
    // Exactly as stored and signed: the federation form, not the client one.
    assert!(pdu.get("signatures").is_some(), "{pdu}");
    assert!(pdu.get("hashes").is_some(), "{pdu}");
    assert!(pdu.get("event_id").is_none(), "{pdu}");
    assert_eq!(
        hs_model::Event::parse(pdu, room.query(|a| a.room_version().clone()).await)
            .unwrap()
            .event_id(),
        message.event_id()
    );

    outbound.stop();
    assert_eq!(h.sender.pending_pdus(), 0);
}

/// `forward_update` on its own, one update at a time, so each event's destinations can be
/// asserted directly: a kick reaches the kicked user's server (the last thing it must hear about
/// its user in the room), and nothing sent after the kick does.
#[tokio::test]
async fn a_kick_reaches_the_kicked_users_server_and_later_events_do_not() {
    let h = Harness::new().await;
    let mut updates = h.rooms.subscribe_global();
    let alice = Harness::alice();
    let bob = h.bob();
    let own = h.identity.server_name.clone();

    let room = h.public_room().await;
    room.membership(
        bob.clone(),
        Action::Join,
        bob.clone(),
        serde_json::json!({}),
        2_000,
    )
    .await
    .expect("bob joins");

    let hello = room
        .send_event(
            alice.clone(),
            "m.room.message".to_owned(),
            None,
            serde_json::json!({ "msgtype": "m.text", "body": "hi" }),
            None,
            3_000,
        )
        .await
        .expect("message");
    let update = update_for(&mut updates, hello.event_id()).await;
    assert_eq!(
        forward_update(&h.rooms, &h.sender, &own, &update)
            .await
            .unwrap(),
        vec![h.remote.clone()]
    );

    let kick = room
        .membership(
            alice.clone(),
            Action::Kick,
            bob.clone(),
            serde_json::json!({}),
            4_000,
        )
        .await
        .expect("kick");
    let update = update_for(&mut updates, kick.event_id()).await;
    assert_eq!(
        forward_update(&h.rooms, &h.sender, &own, &update)
            .await
            .unwrap(),
        vec![h.remote.clone()],
        "the kicked user's server must be told"
    );

    let after = room
        .send_event(
            alice.clone(),
            "m.room.message".to_owned(),
            None,
            serde_json::json!({ "msgtype": "m.text", "body": "after the kick" }),
            None,
            5_000,
        )
        .await
        .expect("message after the kick");
    let update = update_for(&mut updates, after.event_id()).await;
    assert!(
        forward_update(&h.rooms, &h.sender, &own, &update)
            .await
            .unwrap()
            .is_empty(),
        "nobody remote is left to send to"
    );

    // Both queued events arrived, in order, and the message after the kick did not.
    let requests = h.settled_transactions(1).await;
    let pdus: Vec<&serde_json::Value> = requests.iter().flat_map(pdus_of).collect();
    assert_eq!(pdus.len(), 2, "{pdus:?}");
    assert_eq!(pdus[0]["content"]["body"], "hi");
    assert_eq!(pdus[1]["type"], "m.room.member");
    assert_eq!(pdus[1]["state_key"], bob.as_str());
    assert_eq!(pdus[1]["content"]["membership"], "leave");
    assert_eq!(pdus[1]["sender"], alice.as_str());
}
