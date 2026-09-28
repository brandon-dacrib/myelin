//! A forward that lands while a shard is changing hands waits for the new owner instead of
//! failing.
//!
//! Found on two pods on a real cluster (2026-09-28, `docs/status/03-cluster.md`): when one pod
//! drained, the other forwarded to it for about 0.4 s while it answered `421`, and when it came
//! back the survivor forwarded to it for about 1.6 s before it had acquired its shards. With four
//! attempts 10 ms apart every one of those requests reached the client as a `503`. The peer here
//! answers `421` (or `503`) for a set time, then serves; the forwarder, on its default settings,
//! must come through with the `200`.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hs_cluster::config::MeshConfig;
use hs_cluster::mesh::{AuthMode, Envelope, Forwarder, IdempotencyKey};
use hs_cluster::ownership::SingleNode;
use hs_cluster::{Generation, ReplicaId, ShardId, ShardKind, metrics::ClusterMetrics};
use http_body_util::Full;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::time::Instant;

/// A peer that answers `refusal` to every request for `refuse_for` after its first request, then
/// `200 ok`. Counts the requests it saw.
async fn spawn_peer_mid_handoff(refusal: u16, refuse_for: Duration) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    let first = Arc::new(std::sync::OnceLock::<Instant>::new());

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let counter = counter.clone();
            let first = first.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |_req: hyper::Request<hyper::body::Incoming>| {
                        let counter = counter.clone();
                        let first = first.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            let started = *first.get_or_init(Instant::now);
                            let mut response = if started.elapsed() < refuse_for {
                                let mut r = hyper::Response::new(Full::new(Bytes::new()));
                                *r.status_mut() =
                                    http::StatusCode::from_u16(refusal).expect("status");
                                r
                            } else {
                                hyper::Response::new(Full::new(Bytes::from_static(b"ok")))
                            };
                            response.headers_mut().insert(
                                "content-type",
                                http::HeaderValue::from_static("application/octet-stream"),
                            );
                            Ok::<_, Infallible>(response)
                        }
                    },
                );
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    (addr, seen)
}

fn envelope(deadline: Duration) -> Envelope {
    Envelope {
        shard: ShardId::new(ShardKind::Room, 7),
        route: "test.route".into(),
        idempotency_key: IdempotencyKey(42),
        requester: serde_json::Value::Null,
        deadline,
        origin: ReplicaId::new("hs-edge"),
        origin_generation: Generation(1),
        hops: 0,
        traceparent: None,
        payload: Bytes::new(),
    }
}

/// A forwarder on the defaults `hs serve` uses (`MeshConfig::default()`), aimed at `addr`.
fn default_forwarder(addr: String) -> Forwarder {
    let defaults = MeshConfig::default();
    Forwarder::new(
        AuthMode::SharedSecret {
            secret: "test-only-secret".into(),
        },
        None,
        defaults.max_hops,
        defaults.max_attempts,
        defaults.retry_base_backoff,
        SingleNode::new(ReplicaId::new(addr)),
        Arc::new(ClusterMetrics::new()),
    )
    .expect("forwarder")
}

#[tokio::test]
async fn a_forward_rides_out_a_handoff_answered_with_421() {
    // Longer than the 1.6 s measured on the cluster, so the test is about the policy and not a
    // near miss.
    let (addr, seen) = spawn_peer_mid_handoff(421, Duration::from_millis(2_000)).await;
    let forwarder = default_forwarder(addr);
    let started = Instant::now();
    let reply = forwarder
        .forward(envelope(MeshConfig::default().default_deadline))
        .await
        .expect("forward");
    assert_eq!(reply.status, 200, "the forward should wait out the handoff");
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(2_000) && took < Duration::from_millis(2_600),
        "it should succeed within one capped backoff of the handoff ending, took {took:?}"
    );
    let attempts = seen.load(Ordering::SeqCst);
    assert!(
        (6..=20).contains(&attempts),
        "the backoff should double then cap, not spin or give up: {attempts} attempts"
    );
}

#[tokio::test]
async fn a_forward_rides_out_a_peer_answering_503_without_retry_after() {
    let (addr, _seen) = spawn_peer_mid_handoff(503, Duration::from_millis(600)).await;
    let forwarder = default_forwarder(addr);
    let reply = forwarder
        .forward(envelope(MeshConfig::default().default_deadline))
        .await
        .expect("forward");
    assert_eq!(reply.status, 200);
}

#[tokio::test]
async fn a_forward_that_never_settles_gives_up_by_its_deadline() {
    let (addr, _seen) = spawn_peer_mid_handoff(421, Duration::from_secs(3_600)).await;
    let forwarder = default_forwarder(addr);
    let started = Instant::now();
    let reply = forwarder
        .forward(envelope(Duration::from_millis(800)))
        .await
        .expect("a 421 after every retry is a reply, not a transport error");
    assert_eq!(
        reply.status, 421,
        "the last refusal is passed back as it is"
    );
    let took = started.elapsed();
    // 10 + 20 + 40 + 80 + 160 + 250 ms of waits put the sixth refusal at about 560 ms; one
    // more 250 ms wait would cross the 800 ms deadline, so that refusal is the answer.
    assert!(
        took >= Duration::from_millis(500) && took < Duration::from_millis(800),
        "it should stop when the next attempt could not beat the deadline: took {took:?}"
    );
}

/// Knows no owner for any shard until `known_after` has passed since it was built, then names
/// `owner`: a shard released by a draining replica and not yet acquired by the other.
struct OwnerlessFor {
    me: ReplicaId,
    owner: ReplicaId,
    since: Instant,
    known_after: Duration,
}

impl hs_cluster::Ownership for OwnerlessFor {
    fn me(&self) -> &ReplicaId {
        &self.me
    }
    fn owner_of(&self, _shard: ShardId) -> Option<ReplicaId> {
        (self.since.elapsed() >= self.known_after).then(|| self.owner.clone())
    }
    fn is_mine(&self, _shard: ShardId) -> bool {
        false
    }
    fn fence(&self, _shard: ShardId) -> Option<hs_cluster::Fence> {
        None
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<hs_cluster::OwnershipEvent> {
        tokio::sync::broadcast::channel(1).1
    }
    fn shard_map(&self) -> tokio::sync::watch::Receiver<Arc<hs_cluster::ShardMap>> {
        tokio::sync::watch::channel(Arc::new(hs_cluster::ShardMap::default())).1
    }
}

#[tokio::test]
async fn a_forward_waits_while_no_owner_is_known() {
    // Seen in the rolling update: about a second in which the shard had no owner at all.
    let (addr, seen) = spawn_peer_mid_handoff(421, Duration::ZERO).await;
    let defaults = MeshConfig::default();
    let forwarder = Forwarder::new(
        AuthMode::SharedSecret {
            secret: "test-only-secret".into(),
        },
        None,
        defaults.max_hops,
        defaults.max_attempts,
        defaults.retry_base_backoff,
        Arc::new(OwnerlessFor {
            me: ReplicaId::new("hs-edge"),
            owner: ReplicaId::new(addr),
            since: Instant::now(),
            known_after: Duration::from_millis(1_200),
        }),
        Arc::new(ClusterMetrics::new()),
    )
    .expect("forwarder");
    let reply = forwarder
        .forward(envelope(defaults.default_deadline))
        .await
        .expect("the owner turns up before the deadline");
    assert_eq!(reply.status, 200);
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "nothing is sent until an owner is known"
    );
}
