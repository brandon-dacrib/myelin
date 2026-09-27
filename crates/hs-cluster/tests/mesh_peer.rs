//! `POST /mesh/v1/peer` end to end: a real [`hs_cluster::mesh::MeshServer`] on a loopback
//! socket, a real [`hs_cluster::mesh::Forwarder`] dialing it, shared-secret auth on both sides.
//! The replica-to-replica message `hs-user`'s session cluster rides on: what
//! [`hs_cluster::mesh::Forwarder::send_to_peer`] sends is what a [`hs_cluster::mesh::PeerHandler`]
//! receives, with the sender's identity, and a server without a handler is reported as
//! unreachable rather than answered.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hs_cluster::mesh::{
    AuthMode, Envelope, Forwarder, IdempotencyCache, MeshDeps, MeshServer, PeerHandler, Reply,
    ShardHandler, SharedSecretAuthenticator,
};
use hs_cluster::ownership::SingleNode;
use hs_cluster::{Fence, ReplicaId, metrics::ClusterMetrics};
use tokio::sync::watch;

const SECRET: &str = "test-only-mesh-secret";

struct NoShards;

#[async_trait::async_trait]
impl ShardHandler for NoShards {
    async fn handle(&self, _env: Envelope, _fence: Fence) -> Reply {
        Reply {
            status: 500,
            payload: Bytes::from_static(b"this test forwards nothing"),
        }
    }
}

/// Records every message it is handed and echoes the payload back under a status of its own.
struct Recording {
    seen: Mutex<Vec<(ReplicaId, String, Bytes)>>,
}

#[async_trait::async_trait]
impl PeerHandler for Recording {
    async fn handle(&self, from: ReplicaId, route: &str, payload: Bytes) -> Reply {
        self.seen
            .lock()
            .unwrap()
            .push((from, route.to_owned(), payload.clone()));
        Reply {
            status: 202,
            payload,
        }
    }
}

/// Starts a mesh server on a free loopback port and returns its address plus a shutdown handle.
async fn serve(peers: Option<Arc<dyn PeerHandler>>) -> (String, watch::Sender<bool>) {
    // Reserve a port the same way the rest of this workspace's tests do: bind to 0, read the
    // port back, release it, and hand the address to the server. The window in which another
    // process could take it is real and small.
    let addr = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().to_string()
    };
    let deps = Arc::new(MeshDeps {
        authenticator: Arc::new(SharedSecretAuthenticator::new(SECRET)),
        ownership: SingleNode::new(ReplicaId::new(addr.clone())),
        handler: Arc::new(NoShards),
        idempotency: Arc::new(IdempotencyCache::new(Duration::from_secs(1), 16)),
        in_flight: Arc::new(tokio::sync::Semaphore::new(4)),
        nudge: None,
        peers,
    });
    let server = MeshServer::new(addr.clone(), None).unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        server.serve(deps, shutdown_rx).await.unwrap();
    });
    // The listener binds inside `serve`; wait for it to answer rather than for a duration.
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (addr, shutdown_tx)
}

fn forwarder(me: &str, secret: &str) -> Forwarder {
    Forwarder::new(
        AuthMode::SharedSecret {
            secret: secret.to_owned(),
        },
        None,
        3,
        3,
        Duration::from_millis(5),
        SingleNode::new(ReplicaId::new(me)),
        Arc::new(ClusterMetrics::new()),
    )
    .unwrap()
}

#[tokio::test]
async fn a_peer_message_reaches_the_handler_with_the_senders_identity_and_comes_back() {
    let recording = Arc::new(Recording {
        seen: Mutex::new(Vec::new()),
    });
    let (addr, _shutdown) = serve(Some(recording.clone())).await;
    let fwd = forwarder("127.0.0.1:1", SECRET);

    let reply = fwd
        .send_to_peer(
            &ReplicaId::new(addr.clone()),
            "user.wake",
            Bytes::from_static(b"{\"hello\":\"peer\"}"),
            Duration::from_secs(2),
        )
        .await
        .expect("the peer answers");
    assert_eq!(reply.status, 202);
    assert_eq!(&reply.payload[..], b"{\"hello\":\"peer\"}");

    let seen = recording.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (from, route, payload) = &seen[0];
    assert_eq!(
        from.as_str(),
        "127.0.0.1:1",
        "the origin header names the sender"
    );
    assert_eq!(route, "user.wake");
    assert_eq!(&payload[..], b"{\"hello\":\"peer\"}");
}

#[tokio::test]
async fn later_messages_reuse_the_connection_and_a_peer_nobody_listens_on_is_unreachable() {
    let recording = Arc::new(Recording {
        seen: Mutex::new(Vec::new()),
    });
    let (addr, _shutdown) = serve(Some(recording.clone())).await;
    let peer = ReplicaId::new(addr.clone());
    let fwd = forwarder("127.0.0.1:1", SECRET);

    for i in 0..3u8 {
        let reply = fwd
            .send_to_peer(
                &peer,
                "user.positions",
                Bytes::from(vec![i]),
                Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(reply.status, 202);
    }
    assert_eq!(recording.seen.lock().unwrap().len(), 3);

    // A peer that is not there (a port reserved and released, so nothing listens on it) must
    // come back as `PeerUnreachable` promptly, not hang: connection refused is immediate, and
    // the deadline bounds anything slower.
    let nobody = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().to_string()
    };
    let started = std::time::Instant::now();
    let result = fwd
        .send_to_peer(
            &ReplicaId::new(nobody),
            "user.positions",
            Bytes::new(),
            Duration::from_secs(2),
        )
        .await;
    assert!(
        matches!(
            result,
            Err(hs_cluster::error::ForwardError::PeerUnreachable { .. })
        ),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_server_without_a_peer_handler_is_reported_unreachable_not_answered() {
    let (addr, _shutdown) = serve(None).await;
    let fwd = forwarder("127.0.0.1:1", SECRET);
    let result = fwd
        .send_to_peer(
            &ReplicaId::new(addr),
            "user.wake",
            Bytes::new(),
            Duration::from_secs(2),
        )
        .await;
    match result {
        Err(hs_cluster::error::ForwardError::PeerUnreachable { reason, .. }) => {
            assert!(reason.contains("no peer-message handler"), "{reason}");
        }
        other => panic!("expected PeerUnreachable, got {other:?}"),
    }
}

#[tokio::test]
async fn a_peer_message_with_the_wrong_secret_is_refused_before_the_handler_sees_it() {
    let recording = Arc::new(Recording {
        seen: Mutex::new(Vec::new()),
    });
    let (addr, _shutdown) = serve(Some(recording.clone())).await;
    let fwd = forwarder("127.0.0.1:1", "not-the-secret");
    let reply = fwd
        .send_to_peer(
            &ReplicaId::new(addr),
            "user.wake",
            Bytes::new(),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert_eq!(reply.status, 401);
    assert!(recording.seen.lock().unwrap().is_empty());
}
