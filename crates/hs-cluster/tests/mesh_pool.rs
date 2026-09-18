//! An end-to-end check that [`hs_cluster::mesh::Forwarder`] actually pools its HTTP/2 connection
//! per peer rather than dialing fresh every time (`docs/rfcs/0001-cluster-ownership.md` sections 8
//! and 11: "one HTTP/2 connection (multiplexed)" per peer).
//!
//! Nothing in the rest of this crate's test suite exercised the mesh transport over a real
//! socket before this file: the chaos harness and the lib's own unit tests all call `Ownership`
//! and `ChaosLog` directly, bypassing HTTP entirely. This uses a minimal raw HTTP/2 peer (not
//! `MeshServer`, so the connection count measured is unambiguously about `Forwarder`'s own
//! behavior, not any detail of the real server) that counts how many separate TCP connections it
//! ever accepts while `Forwarder` sends it several requests in a row.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hs_cluster::mesh::{AuthMode, Envelope, IdempotencyKey};
use hs_cluster::ownership::SingleNode;
use hs_cluster::{Generation, ReplicaId, ShardId, ShardKind, metrics::ClusterMetrics};
use http_body_util::Full;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

async fn spawn_counting_peer() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    |_req: hyper::Request<hyper::body::Incoming>| async move {
                        Ok::<_, Infallible>(hyper::Response::new(Full::new(Bytes::from_static(
                            b"ok",
                        ))))
                    },
                );
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    (addr, accepted)
}

/// Like [`spawn_counting_peer`], but the *first* accepted connection is forcibly closed a short
/// time after it is accepted (regardless of activity), simulating a peer that restarted or an
/// idle connection that timed out from the server's side. Every later connection is served
/// normally and indefinitely. Used to test that [`hs_cluster::mesh::Forwarder`] notices a dead
/// pooled connection and redials rather than failing outright.
async fn spawn_peer_whose_first_connection_dies() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    |_req: hyper::Request<hyper::body::Incoming>| async move {
                        Ok::<_, Infallible>(hyper::Response::new(Full::new(Bytes::from_static(
                            b"ok",
                        ))))
                    },
                );
                let conn = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service);
                if index == 0 {
                    // Drop the connection shortly after accepting it, however busy or idle it
                    // is, so the client-side pooled handle becomes unusable without either side
                    // ever seeing a clean HTTP-level close.
                    let _ = tokio::time::timeout(Duration::from_millis(30), conn).await;
                } else {
                    let _ = conn.await;
                }
            });
        }
    });

    (addr, accepted)
}

fn envelope(seq: u128) -> Envelope {
    Envelope {
        shard: ShardId::new(ShardKind::Room, 0),
        route: "test.route".into(),
        idempotency_key: IdempotencyKey(seq),
        requester: serde_json::Value::Null,
        deadline: Duration::from_secs(5),
        origin: ReplicaId::new("hs-client"),
        origin_generation: Generation(1),
        hops: 0,
        traceparent: None,
        payload: Bytes::new(),
    }
}

#[tokio::test]
async fn forwarder_reuses_one_connection_across_many_forwards() {
    let (addr, accepted) = spawn_counting_peer().await;

    // `SingleNode`'s `owner_of` always returns the `ReplicaId` it was built with; using the
    // peer's own address as that id is exactly `Forwarder::resolve_addr`'s documented
    // convention (a `ReplicaId` doubling as `host:port`), and lets this test reuse `SingleNode`
    // instead of writing a bespoke `Ownership` implementation -- `is_mine`/`fence` are never
    // called by `Forwarder`, so `SingleNode`'s single-node semantics for them are irrelevant
    // here.
    let ownership = SingleNode::new(ReplicaId::new(addr));

    let forwarder = hs_cluster::mesh::Forwarder::new(
        AuthMode::SharedSecret {
            secret: "test-only-secret".into(),
        },
        None,
        3,
        4,
        Duration::from_millis(5),
        ownership,
        Arc::new(ClusterMetrics::new()),
    )
    .expect("forwarder should build without TLS material in shared-secret mode");

    for seq in 0..5u128 {
        let reply = forwarder
            .forward(envelope(seq))
            .await
            .expect("forward should succeed");
        assert_eq!(
            reply.status, 200,
            "peer should have answered 200 for every request"
        );
    }

    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "the forwarder should have reused one pooled connection across all 5 forwards, not dialed fresh every time"
    );
}

#[tokio::test]
async fn forwarder_redials_after_the_pooled_connection_is_gone() {
    let (addr, accepted) = spawn_peer_whose_first_connection_dies().await;
    let ownership = SingleNode::new(ReplicaId::new(addr));
    let forwarder = hs_cluster::mesh::Forwarder::new(
        AuthMode::SharedSecret {
            secret: "test-only-secret".into(),
        },
        None,
        3,
        4,
        Duration::from_millis(5),
        ownership,
        Arc::new(ClusterMetrics::new()),
    )
    .unwrap();

    let reply = forwarder
        .forward(envelope(1))
        .await
        .expect("first forward should succeed");
    assert_eq!(reply.status, 200);
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "first forward should have dialed exactly one connection"
    );

    // Give the peer's 30ms self-close timer time to fire, so the pooled connection is now dead
    // on the server side without either end having done a clean HTTP-level close.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let reply2 = forwarder
        .forward(envelope(2))
        .await
        .expect("forward should succeed by redialing after the pooled connection died");
    assert_eq!(reply2.status, 200);
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        2,
        "the forwarder should have noticed the dead pooled connection, evicted it, and dialed exactly one fresh one"
    );
}
