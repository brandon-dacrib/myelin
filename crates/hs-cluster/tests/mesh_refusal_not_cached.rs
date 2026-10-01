//! A `503` the owner's handler answered is not remembered as the forward's result.
//!
//! The mesh server keeps each forward's reply under its idempotency key, so that a retry of a
//! forward that already did its work is answered from the cache instead of doing it twice. A
//! `503` is not work done: `hs-room`'s fence answers it when ownership moved under the write and
//! nothing was committed. The forwarder retries a `503` with the same key (decision 0017), and
//! while the `503` was cached every retry got it back from the cache until the forward's
//! deadline, even once the handler would have succeeded: a forwarded write fenced once was
//! failed for good. Seen as a flaky `503` on a forwarded send in
//! `crates/hs-cli/tests/cluster_create_room.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hs_cluster::config::MeshConfig;
use hs_cluster::mesh::{
    AuthMode, Envelope, Forwarder, IdempotencyCache, IdempotencyKey, MeshDeps, MeshServer, Reply,
    ShardHandler, SharedSecretAuthenticator,
};
use hs_cluster::ownership::SingleNode;
use hs_cluster::{Fence, Generation, ReplicaId, ShardId, ShardKind, metrics::ClusterMetrics};
use tokio::sync::watch;

const SECRET: &str = "test-only-mesh-secret";

/// Answers `503` to its first `refusals` calls and `200 done` after, counting every call.
struct FencedThenServes {
    refusals: usize,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ShardHandler for FencedThenServes {
    async fn handle(&self, _env: Envelope, _fence: Fence) -> Reply {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.refusals {
            Reply {
                status: 503,
                payload: Bytes::from_static(b"fenced"),
            }
        } else {
            Reply::ok(Bytes::from_static(b"done"))
        }
    }
}

async fn serve(handler: Arc<FencedThenServes>) -> (String, watch::Sender<bool>) {
    let addr = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().to_string()
    };
    let deps = Arc::new(MeshDeps {
        authenticator: Arc::new(SharedSecretAuthenticator::new(SECRET)),
        ownership: SingleNode::new(ReplicaId::new(addr.clone())),
        handler,
        idempotency: Arc::new(IdempotencyCache::new(Duration::from_secs(60), 16)),
        in_flight: Arc::new(tokio::sync::Semaphore::new(4)),
        nudge: None,
        peers: None,
    });
    let server = MeshServer::new(addr.clone(), None).unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        server.serve(deps, shutdown_rx).await.unwrap();
    });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (addr, shutdown_tx)
}

fn envelope(key: u128) -> Envelope {
    Envelope {
        shard: ShardId::new(ShardKind::Room, 3),
        route: "test.route".into(),
        idempotency_key: IdempotencyKey(key),
        requester: serde_json::Value::Null,
        deadline: Duration::from_secs(5),
        origin: ReplicaId::new("hs-edge"),
        origin_generation: Generation(1),
        hops: 0,
        traceparent: None,
        payload: Bytes::new(),
    }
}

#[tokio::test]
async fn a_forward_fenced_twice_succeeds_once_the_handler_does() {
    let handler = Arc::new(FencedThenServes {
        refusals: 2,
        calls: AtomicUsize::new(0),
    });
    let (addr, _shutdown) = serve(handler.clone()).await;
    let defaults = MeshConfig::default();
    let forwarder = Forwarder::new(
        AuthMode::SharedSecret {
            secret: SECRET.into(),
        },
        None,
        defaults.max_hops,
        defaults.max_attempts,
        Duration::from_millis(5),
        SingleNode::new(ReplicaId::new(addr)),
        Arc::new(ClusterMetrics::new()),
    )
    .unwrap();

    let reply = forwarder.forward(envelope(7)).await.expect("forwarded");
    assert_eq!(
        (reply.status, &reply.payload[..]),
        (200, &b"done"[..]),
        "the retries reached the handler instead of a cached 503"
    );
    assert_eq!(handler.calls.load(Ordering::SeqCst), 3);

    // A success is still cached: the same key again does not run the handler a fourth time.
    let again = forwarder.forward(envelope(7)).await.expect("forwarded");
    assert_eq!(again.status, 200);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 3);
}
