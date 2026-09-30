//! `/sync` across replicas: `hs-user`'s [`SessionCluster`] over the real mesh, and the mesh's
//! [`PeerHandler`] that feeds the session hub.
//!
//! `hs_user::cluster`'s module docs are the design; this module is only the transport. Two
//! messages travel over `POST /mesh/v1/peer` ([`hs_cluster::mesh::Forwarder::send_to_peer`]):
//!
//! - `user.wake`: a [`WakeBatch`], JSON, from a room owner's hub to every other live replica
//!   (the shard map's distinct owners, [`hs_cluster::ownership::ShardMap::replicas`]). One
//!   pump task per peer drains an unbounded queue into one batch per round trip, so a burst
//!   on the owner costs the peer one message, not one per event, and a slow peer only delays
//!   its own batches. Delivery is best effort: a batch that cannot be sent is logged and
//!   dropped, because the store is what a reader trusts (`hs_user::cluster::RoomMirror`) and
//!   the long-poll's own periodic re-check bounds the damage at half a second. The same batch
//!   carries the typing, receipt and presence changes made on this replica since the last one
//!   ([`EphemeralUpdate`]; `hs_user::cluster`'s module docs say what a receiver does with
//!   them), from whichever replica took the change, owner or not.
//! - `user.positions`: an empty request, answered with a [`PeerPosition`] -- this replica's
//!   key and its registry's published number -- which the asking replica's
//!   `SessionHub::settle_before_read` waits to have received the wakes up to.
//!
//! The key a replica sends under is `replica#generation`: a restarted replica numbers its
//! stream from zero again, and its peers must not hold it to the mark its previous
//! incarnation reached.
//!
//! `hs_cluster_ephemeral_updates_total{kind, direction}` counts the typing, receipt and
//! presence updates this replica sent to peers (`direction="sent"`, once per peer they reached)
//! and received from them (`direction="received"`).
//!
//! [`install`] wires all of it into a hub and a [`crate::cluster::ClusterHandles`]; in
//! single-node mode it does nothing at all, and the hub behaves as it always has.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hs_cluster::mesh::{Forwarder, PeerHandler, Reply};
use hs_cluster::{Ownership, ReplicaId, ShardLayout};
use hs_kv::KvBackend;
use hs_user::cluster::{
    EphemeralUpdate, PeerPosition, RoomMirror, RoomWake, SessionCluster, WakeBatch,
};
use hs_user::hub::SessionHub;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use ruma::RoomId;
use tokio::sync::mpsc;

/// The mesh route a [`WakeBatch`] travels on.
pub const WAKE_ROUTE: &str = "user.wake";
/// The mesh route a [`PeerPosition`] is asked for on.
pub const POSITIONS_ROUTE: &str = "user.positions";
/// How long one wake batch may take to reach a peer before the pump gives up on it and moves
/// on to the next. Generous: the pump is the only thing waiting, and a batch that is late is
/// still worth more than one that is dropped.
const WAKE_DEADLINE: Duration = Duration::from_secs(2);

/// The labels of `hs_cluster_ephemeral_updates_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct EphemeralLabels {
    /// `typing`, `receipt` or `presence` ([`EphemeralUpdate::kind`]).
    kind: &'static str,
    /// `sent` or `received`.
    direction: &'static str,
}

/// The counters this module exports. See the module docs.
#[derive(Clone, Default)]
pub struct SyncClusterMetrics {
    ephemeral: Family<EphemeralLabels, Counter>,
}

impl SyncClusterMetrics {
    /// Registers the family into `metrics`'s shared registry.
    #[must_use]
    pub fn register(metrics: &hs_telemetry::metrics::Metrics) -> Self {
        let this = Self::default();
        metrics.with_registry(|registry| {
            // Registered without `_total`: the text encoder appends it.
            registry.register(
                "hs_cluster_ephemeral_updates",
                "Typing, receipt and presence updates exchanged with the other replicas, by \
                 kind and direction (sent: once per peer reached; received)",
                this.ephemeral.clone(),
            );
        });
        this
    }

    fn count(&self, updates: &[EphemeralUpdate], direction: &'static str) {
        for update in updates {
            self.ephemeral
                .get_or_create(&EphemeralLabels {
                    kind: update.kind(),
                    direction,
                })
                .inc();
        }
    }
}

/// What a pump's queue carries: the two things a batch is made of.
enum Outbound {
    Wake(RoomWake),
    Ephemeral(EphemeralUpdate),
}

/// `hs-user`'s view of the cluster, over the mesh. See the module docs.
pub struct MeshSessionCluster {
    forwarder: Arc<Forwarder>,
    ownership: Arc<dyn Ownership>,
    layout: ShardLayout,
    me: ReplicaId,
    me_key: String,
    metrics: SyncClusterMetrics,
    /// One queue per peer, drained by that peer's pump task. A `std::sync::Mutex`: every
    /// critical section is a map lookup with no `.await` inside.
    queues: Mutex<HashMap<ReplicaId, mpsc::UnboundedSender<Outbound>>>,
}

impl MeshSessionCluster {
    /// Builds the cluster view for a replica whose key is `me_key`, counting into `metrics`.
    #[must_use]
    pub fn new(
        forwarder: Arc<Forwarder>,
        ownership: Arc<dyn Ownership>,
        layout: ShardLayout,
        me_key: String,
        metrics: SyncClusterMetrics,
    ) -> Self {
        let me = ownership.me().clone();
        Self {
            forwarder,
            ownership,
            layout,
            me,
            me_key,
            metrics,
            queues: Mutex::new(HashMap::new()),
        }
    }

    /// Queues `item` for every other live replica.
    fn send_to_peers(&self, item: impl Fn() -> Outbound) {
        let peers = self.peers();
        for peer in &peers {
            // A closed queue means the pump ended, which it only does when the queue was
            // dropped from the map; the next call starts a new one.
            let _ = self.queue_for(peer, &peers).send(item());
        }
    }

    /// Every other replica that owns at least one shard right now.
    fn peers(&self) -> Vec<ReplicaId> {
        self.ownership
            .shard_map()
            .borrow()
            .replicas()
            .into_iter()
            .filter(|peer| *peer != self.me)
            .collect()
    }

    /// The queue for `peer`, starting its pump if this is the first wake for it. Queues for
    /// peers no longer in `peers` are dropped here too, which ends their pumps.
    fn queue_for(&self, peer: &ReplicaId, peers: &[ReplicaId]) -> mpsc::UnboundedSender<Outbound> {
        let mut queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queues.retain(|known, _| peers.contains(known));
        queues
            .entry(peer.clone())
            .or_insert_with(|| {
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(pump(
                    self.forwarder.clone(),
                    peer.clone(),
                    self.me_key.clone(),
                    self.metrics.clone(),
                    rx,
                ));
                tx
            })
            .clone()
    }
}

/// Drains `rx` into one [`WakeBatch`] per mesh round trip to `peer`, until the queue's sender
/// is dropped.
async fn pump(
    forwarder: Arc<Forwarder>,
    peer: ReplicaId,
    me_key: String,
    metrics: SyncClusterMetrics,
    mut rx: mpsc::UnboundedReceiver<Outbound>,
) {
    fn fold(batch: &mut WakeBatch, item: Outbound) {
        match item {
            Outbound::Wake(wake) => batch.push(wake),
            Outbound::Ephemeral(update) => batch.push_ephemeral(update),
        }
    }
    while let Some(first) = rx.recv().await {
        let mut batch = WakeBatch::new(&me_key);
        fold(&mut batch, first);
        while let Ok(more) = rx.try_recv() {
            fold(&mut batch, more);
        }
        if batch.is_empty() {
            continue;
        }
        let payload = match serde_json::to_vec(&batch) {
            Ok(payload) => Bytes::from(payload),
            Err(error) => {
                tracing::warn!(%error, "could not encode a wake batch; dropping it");
                continue;
            }
        };
        match forwarder
            .send_to_peer(&peer, WAKE_ROUTE, payload, WAKE_DEADLINE)
            .await
        {
            Ok(reply) if reply.status == 200 => {
                metrics.count(&batch.ephemeral, "sent");
                tracing::debug!(
                    %peer,
                    consumed = batch.consumed,
                    rooms = batch.wakes.len(),
                    ephemeral = batch.ephemeral.len(),
                    "sent a wake batch to a peer"
                );
            }
            Ok(reply) => tracing::debug!(
                %peer,
                status = reply.status,
                "a peer refused a wake batch; its long-polls will fall back to their re-check"
            ),
            Err(error) => tracing::debug!(
                %peer,
                %error,
                "a wake batch did not reach a peer; its long-polls will fall back to their re-check"
            ),
        }
    }
}

#[async_trait::async_trait]
impl SessionCluster for MeshSessionCluster {
    fn owns_room(&self, room_id: &RoomId) -> bool {
        self.ownership
            .is_mine(self.layout.room_shard(room_id.as_str()))
    }

    fn publish(&self, wake: RoomWake) {
        self.send_to_peers(|| Outbound::Wake(wake.clone()));
    }

    fn publish_ephemeral(&self, update: EphemeralUpdate) {
        self.send_to_peers(|| Outbound::Ephemeral(update.clone()));
    }

    async fn peer_positions(&self, deadline: Duration) -> Vec<PeerPosition> {
        let mut asks = tokio::task::JoinSet::new();
        for peer in self.peers() {
            let forwarder = self.forwarder.clone();
            asks.spawn(async move {
                match forwarder
                    .send_to_peer(&peer, POSITIONS_ROUTE, Bytes::new(), deadline)
                    .await
                {
                    Ok(reply) if reply.status == 200 => {
                        serde_json::from_slice::<PeerPosition>(&reply.payload).ok()
                    }
                    Ok(reply) => {
                        tracing::debug!(%peer, status = reply.status, "a peer refused a positions request");
                        None
                    }
                    Err(error) => {
                        tracing::debug!(%peer, %error, "a peer did not answer a positions request");
                        None
                    }
                }
            });
        }
        let mut positions = Vec::new();
        while let Some(joined) = asks.join_next().await {
            if let Ok(Some(position)) = joined {
                positions.push(position);
            }
        }
        positions
    }
}

/// The receiving side: answers `user.wake` and `user.positions` for one hub.
pub struct SessionPeerHandler<B: KvBackend, R: hs_user::room_source::RoomSource<B>> {
    hub: Arc<SessionHub<B, R>>,
    me_key: String,
    metrics: SyncClusterMetrics,
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static, R: hs_user::room_source::RoomSource<B> + 'static> PeerHandler
    for SessionPeerHandler<B, R>
{
    async fn handle(&self, from: ReplicaId, route: &str, payload: Bytes) -> Reply {
        match route {
            WAKE_ROUTE => match serde_json::from_slice::<WakeBatch>(&payload) {
                Ok(batch) => {
                    self.metrics.count(&batch.ephemeral, "received");
                    self.hub.receive_wakes(batch).await;
                    Reply::ok(Bytes::new())
                }
                Err(error) => Reply {
                    status: 400,
                    payload: Bytes::from(format!("bad wake batch from {from}: {error}")),
                },
            },
            POSITIONS_ROUTE => {
                let position = PeerPosition {
                    peer: self.me_key.clone(),
                    published: self.hub.rooms().global_published_seq(),
                };
                match serde_json::to_vec(&position) {
                    Ok(body) => Reply::ok(Bytes::from(body)),
                    Err(error) => Reply {
                        status: 500,
                        payload: Bytes::from(format!("encoding a position: {error}")),
                    },
                }
            }
            other => Reply {
                status: 404,
                payload: Bytes::from(format!("no such peer route: {other}")),
            },
        }
    }
}

/// Makes `hub` cluster-aware over `handles`' mesh: installs the [`MeshSessionCluster`] and a
/// [`RoomMirror`] on the hub, and this replica's [`SessionPeerHandler`] on the handles, for
/// [`crate::cluster::ClusterHandles::spawn_mesh`] to serve; registers this module's counters
/// into `metrics`. Call it after the cluster has started and before the mesh listener is
/// spawned. A no-op in single-node mode (no forwarder, so no peers to speak to; nothing is
/// registered either).
///
/// # Errors
/// Returns [`hs_kv::KvError`] if the mirror could not open `hs-room`'s keyspaces.
pub fn install<B: KvBackend + 'static>(
    hub: &Arc<SessionHub<B, Arc<hs_room::registry::RoomRegistry<B>>>>,
    handles: &crate::cluster::ClusterHandles,
    backend: B,
    identity: hs_room::identity::HomeserverIdentity,
    metrics: &hs_telemetry::metrics::Metrics,
) -> Result<(), hs_kv::KvError> {
    let Some(forwarder) = handles.forwarder.clone() else {
        return Ok(());
    };
    let metrics = SyncClusterMetrics::register(metrics);
    let me_key = format!("{}#{}", handles.origin(), handles.origin_generation().0);
    let cluster = Arc::new(MeshSessionCluster::new(
        forwarder,
        handles.cluster.ownership().clone(),
        handles.layout,
        me_key.clone(),
        metrics.clone(),
    ));
    let mirror = Arc::new(RoomMirror::open(backend, identity)?);
    hub.install_cluster(cluster, mirror);
    handles.add_peer_handler(
        "user.",
        Arc::new(SessionPeerHandler {
            hub: hub.clone(),
            me_key: me_key.clone(),
            metrics,
        }),
    );
    tracing::info!(
        replica = %me_key,
        "/sync is cluster-aware: room owners wake this replica's long-polls over the mesh, and \
         typing, receipts and presence cross it"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_cluster::ownership::SingleNode;
    use hs_kv::memory::MemoryBackend;
    use hs_room::identity::HomeserverIdentity;
    use hs_room::registry::RoomRegistry;
    use hs_user::store::tables::TablesUserStore;

    type Registry = Arc<RoomRegistry<MemoryBackend>>;

    fn hub() -> Arc<SessionHub<MemoryBackend, Registry>> {
        let backend = MemoryBackend::new();
        let registry: Registry = Arc::new(
            RoomRegistry::open(backend.clone(), HomeserverIdentity::for_tests("peer.test"))
                .unwrap(),
        );
        let store: hs_user::store::DynUserStore = Arc::new(TablesUserStore::open(backend).unwrap());
        Arc::new(SessionHub::new(store, registry, 500))
    }

    #[tokio::test]
    async fn the_peer_handler_answers_positions_with_this_replicas_key_and_number() {
        let hub = hub();
        let handler = SessionPeerHandler {
            hub: hub.clone(),
            me_key: "127.0.0.1:1#7".to_owned(),
            metrics: SyncClusterMetrics::default(),
        };
        let reply = handler
            .handle(ReplicaId::new("peer"), POSITIONS_ROUTE, Bytes::new())
            .await;
        assert_eq!(reply.status, 200);
        let position: PeerPosition = serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(position.peer, "127.0.0.1:1#7");
        assert_eq!(position.published, hub.rooms().global_published_seq());
    }

    #[tokio::test]
    async fn the_peer_handler_records_a_wake_batchs_mark_and_rejects_junk() {
        let hub = hub();
        let handler = SessionPeerHandler {
            hub: hub.clone(),
            me_key: "me#1".to_owned(),
            metrics: SyncClusterMetrics::default(),
        };
        let mut batch = WakeBatch::new("a#3");
        batch.push(RoomWake {
            room_id: ruma::room_id!("!r:peer.test").to_owned(),
            room_pos: 4,
            global_seq: 9,
            users: vec![ruma::user_id!("@alice:peer.test").to_owned()],
        });
        let reply = handler
            .handle(
                ReplicaId::new("a"),
                WAKE_ROUTE,
                Bytes::from(serde_json::to_vec(&batch).unwrap()),
            )
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(hub.peer_consumed("a#3"), 9);

        let junk = handler
            .handle(ReplicaId::new("a"), WAKE_ROUTE, Bytes::from_static(b"{"))
            .await;
        assert_eq!(junk.status, 400);
        let unknown = handler
            .handle(ReplicaId::new("a"), "user.nope", Bytes::new())
            .await;
        assert_eq!(unknown.status, 404);
    }

    /// A peer's typing update is applied to this hub's registry and counted as received; a
    /// batch with nothing but ephemeral updates is a batch, and moves no mark.
    #[tokio::test]
    async fn the_peer_handler_applies_a_batchs_ephemeral_updates_and_counts_them() {
        let hub = hub();
        let metrics = SyncClusterMetrics::default();
        let handler = SessionPeerHandler {
            hub: hub.clone(),
            me_key: "me#1".to_owned(),
            metrics: metrics.clone(),
        };
        let room = ruma::room_id!("!r:peer.test").to_owned();
        let alice = ruma::user_id!("@alice:peer.test").to_owned();
        let mut batch = WakeBatch::new("a#3");
        batch.push_ephemeral(EphemeralUpdate::Typing {
            room_id: room.clone(),
            user_id: alice.clone(),
            typing: true,
            timeout_ms: 30_000,
        });
        batch.push_ephemeral(EphemeralUpdate::Presence {
            user_id: alice.clone(),
            seq: 1,
        });
        let reply = handler
            .handle(
                ReplicaId::new("a"),
                WAKE_ROUTE,
                Bytes::from(serde_json::to_vec(&batch).unwrap()),
            )
            .await;
        assert_eq!(reply.status, 200);
        // The room does not exist here, so nobody was woken -- but the typing was recorded.
        let (typing, seq) = hub.typing_users(&room).await;
        assert_eq!(typing, vec![alice]);
        assert!(seq > 0);
        assert_eq!(hub.peer_consumed("a#3"), 0);
        let received = |kind| {
            metrics
                .ephemeral
                .get_or_create(&EphemeralLabels {
                    kind,
                    direction: "received",
                })
                .get()
        };
        assert_eq!(received("typing"), 1);
        assert_eq!(received("presence"), 1);
        assert_eq!(received("receipt"), 0);
    }

    #[test]
    fn a_single_node_replica_owns_every_room_and_has_no_peers() {
        let ownership: Arc<dyn Ownership> = SingleNode::new(ReplicaId::new("solo:1"));
        let forwarder = Arc::new(
            Forwarder::new(
                hs_cluster::mesh::AuthMode::SharedSecret { secret: "s".into() },
                None,
                1,
                1,
                Duration::from_millis(1),
                ownership.clone(),
                Arc::new(hs_cluster::metrics::ClusterMetrics::new()),
            )
            .unwrap(),
        );
        let cluster = MeshSessionCluster::new(
            forwarder,
            ownership,
            ShardLayout::default(),
            "solo:1#1".to_owned(),
            SyncClusterMetrics::default(),
        );
        assert!(cluster.owns_room(ruma::room_id!("!any:peer.test")));
        assert!(cluster.peers().is_empty());
    }
}
