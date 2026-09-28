//! EDUs in a cluster: the replica that takes a user's typing, receipt, presence or to-device
//! request hands each EDU for a destination another replica sends for to that replica, over the
//! mesh, instead of dropping it.
//!
//! Which replica sends to a destination is decided by the owner of its federation shard
//! (`crate::federation_sender::ShardGate`). PDUs need nothing more: they are written to the
//! shared outbound store, and the owner reads them from there. EDUs are in memory only
//! (`hs_federation::sender`'s module docs, "EDUs"), so the one replica that knows of one --
//! the one the client spoke to -- has to give it to the owner:
//!
//! - [`MeshEduForwarder`] is the sender's `hs_federation::sender::EduForwarder`. It looks up
//!   the owner of `ShardLayout::federation_shard(destination)` and queues the EDU for it; one
//!   pump task per owner drains its queue into one `federation.edu` message per round trip
//!   ([`hs_cluster::mesh::Forwarder::send_to_peer`]), in order, so to-device messages keep
//!   theirs.
//! - [`EduPeerHandler`] answers `federation.edu` on the owner by queueing each EDU with
//!   `FederationSender::enqueue_edu_local`, which never forwards again: an EDU for a
//!   destination the owner has meanwhile handed on is dropped rather than bounced around.
//!
//! The device-list announcer does not forward (it queues with `enqueue_edu_local`): every
//! replica follows the same device-list stream, so the owner of each destination announces to
//! it already.
//!
//! Delivery is best effort, as an EDU's is anyway. What happens to each is counted in
//! `hs_federation_edus_forwarded_total{edu_type,outcome}` (`forwarded` or `failed` on the
//! replica that forwards, `received` on the owner) and logged at debug level, a failure at
//! warn.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use hs_cluster::mesh::{Forwarder, PeerHandler, Reply};
use hs_cluster::{Ownership, ReplicaId, ShardLayout};
use hs_federation::metrics::{EduForwardOutcome, EduMetrics};
use hs_federation::sender::{EduForwarder, FederationSender};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

/// The mesh route forwarded EDUs travel on, and the prefix [`install`] adds the handler for.
pub const EDU_ROUTE: &str = "federation.edu";
/// The route prefix [`EduPeerHandler`] is added for.
pub const ROUTE_PREFIX: &str = "federation.";
/// How long one batch may take to reach the owner before it is counted as failed.
const FORWARD_DEADLINE: Duration = Duration::from_secs(2);

/// One EDU as it travels to the replica that sends for its destination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardedEdu {
    /// The server the EDU is for.
    pub destination: String,
    /// `edu_type`.
    pub edu_type: String,
    /// `content`.
    pub content: Value,
    /// The sender's coalescing key, if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coalesce_key: Option<String>,
}

/// The sender's [`EduForwarder`] over the mesh. See the module docs.
pub struct MeshEduForwarder {
    forwarder: Arc<Forwarder>,
    ownership: Arc<dyn Ownership>,
    layout: ShardLayout,
    metrics: EduMetrics,
    /// One queue per owner, drained by that owner's pump task. A `std::sync::Mutex`: every
    /// critical section is a map lookup with no `.await` inside.
    queues: Mutex<HashMap<ReplicaId, mpsc::UnboundedSender<ForwardedEdu>>>,
}

impl MeshEduForwarder {
    /// A forwarder that finds owners through `ownership` and `layout` and reaches them through
    /// `forwarder`, counting into `metrics`.
    #[must_use]
    pub fn new(
        forwarder: Arc<Forwarder>,
        ownership: Arc<dyn Ownership>,
        layout: ShardLayout,
        metrics: EduMetrics,
    ) -> Self {
        Self {
            forwarder,
            ownership,
            layout,
            metrics,
            queues: Mutex::new(HashMap::new()),
        }
    }

    fn queue_for(&self, owner: &ReplicaId) -> mpsc::UnboundedSender<ForwardedEdu> {
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(queue) = queues.get(owner)
            && !queue.is_closed()
        {
            return queue.clone();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(pump(
            self.forwarder.clone(),
            owner.clone(),
            self.metrics.clone(),
            rx,
        ));
        queues.insert(owner.clone(), tx.clone());
        tx
    }
}

impl EduForwarder for MeshEduForwarder {
    fn forward_edu(
        &self,
        destination: &str,
        edu_type: &str,
        content: &Value,
        coalesce_key: Option<&str>,
    ) {
        let shard = self.layout.federation_shard(destination);
        let owner = self
            .ownership
            .owner_of(shard)
            .filter(|owner| owner != self.ownership.me());
        let Some(owner) = owner else {
            // Nobody else owns it (a handoff in progress): nothing to hand it to.
            tracing::warn!(
                destination,
                edu_type,
                "no other replica owns this destination's federation shard; dropping an EDU"
            );
            self.metrics
                .record_forwarded(edu_type, EduForwardOutcome::Failed);
            return;
        };
        let edu = ForwardedEdu {
            destination: destination.to_owned(),
            edu_type: edu_type.to_owned(),
            content: content.clone(),
            coalesce_key: coalesce_key.map(str::to_owned),
        };
        if self.queue_for(&owner).send(edu).is_err() {
            self.metrics
                .record_forwarded(edu_type, EduForwardOutcome::Failed);
        }
    }
}

/// Drains `rx` into one `federation.edu` message per round trip to `owner`, in order, until
/// the queue's sender is dropped.
async fn pump(
    forwarder: Arc<Forwarder>,
    owner: ReplicaId,
    metrics: EduMetrics,
    mut rx: mpsc::UnboundedReceiver<ForwardedEdu>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        let payload = match serde_json::to_vec(&batch) {
            Ok(payload) => Bytes::from(payload),
            Err(error) => {
                tracing::warn!(%error, "could not encode forwarded EDUs; dropping them");
                count(&metrics, &batch, EduForwardOutcome::Failed);
                continue;
            }
        };
        let outcome = match forwarder
            .send_to_peer(&owner, EDU_ROUTE, payload, FORWARD_DEADLINE)
            .await
        {
            Ok(reply) if reply.status == 200 => {
                tracing::debug!(%owner, edus = batch.len(), "forwarded EDUs to the replica that sends for their destinations");
                EduForwardOutcome::Forwarded
            }
            Ok(reply) => {
                tracing::warn!(%owner, status = reply.status, edus = batch.len(), "the replica that sends for these destinations refused forwarded EDUs");
                EduForwardOutcome::Failed
            }
            Err(error) => {
                tracing::warn!(%owner, %error, edus = batch.len(), "forwarded EDUs did not reach the replica that sends for their destinations");
                EduForwardOutcome::Failed
            }
        };
        count(&metrics, &batch, outcome);
    }
}

fn count(metrics: &EduMetrics, batch: &[ForwardedEdu], outcome: EduForwardOutcome) {
    for edu in batch {
        metrics.record_forwarded(&edu.edu_type, outcome);
    }
}

/// The owner's side: answers `federation.edu` by queueing each EDU here, never forwarding it
/// again. See the module docs.
pub struct EduPeerHandler {
    sender: Arc<FederationSender>,
    metrics: EduMetrics,
}

impl EduPeerHandler {
    /// A handler that queues on `sender` and counts into `metrics`.
    #[must_use]
    pub fn new(sender: Arc<FederationSender>, metrics: EduMetrics) -> Self {
        Self { sender, metrics }
    }
}

#[async_trait::async_trait]
impl PeerHandler for EduPeerHandler {
    async fn handle(&self, from: ReplicaId, route: &str, payload: Bytes) -> Reply {
        if route != EDU_ROUTE {
            return Reply {
                status: 404,
                payload: Bytes::from(format!("no such peer route: {route}")),
            };
        }
        let batch: Vec<ForwardedEdu> = match serde_json::from_slice(&payload) {
            Ok(batch) => batch,
            Err(error) => {
                return Reply {
                    status: 400,
                    payload: Bytes::from(format!("bad forwarded EDUs from {from}: {error}")),
                };
            }
        };
        tracing::debug!(%from, edus = batch.len(), "EDUs forwarded by a peer");
        for edu in batch {
            self.metrics
                .record_forwarded(&edu.edu_type, EduForwardOutcome::Received);
            self.sender.enqueue_edu_local(
                [edu.destination],
                &edu.edu_type,
                edu.content,
                edu.coalesce_key,
            );
        }
        Reply::ok(Bytes::new())
    }
}

/// Makes `sender` forward EDUs for destinations another replica sends for, over `handles`'
/// mesh, and answers the same from peers. Call it before
/// [`crate::cluster::ClusterHandles::spawn_mesh`]. A no-op in single-node mode (no forwarder:
/// every destination is sent for here).
pub fn install(handles: &crate::cluster::ClusterHandles, sender: &Arc<FederationSender>) {
    let Some(forwarder) = handles.forwarder.clone() else {
        return;
    };
    let metrics = sender.edu_metrics().unwrap_or_default();
    sender.install_edu_forwarder(Arc::new(MeshEduForwarder::new(
        forwarder,
        handles.cluster.ownership().clone(),
        handles.layout,
        metrics.clone(),
    )));
    handles.add_peer_handler(
        ROUTE_PREFIX,
        Arc::new(EduPeerHandler::new(sender.clone(), metrics)),
    );
    tracing::info!(
        "EDUs for destinations another replica sends for are forwarded to it over the mesh"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forwarded_edu_round_trips_through_json() {
        let edu = ForwardedEdu {
            destination: "remote.example".to_owned(),
            edu_type: "m.typing".to_owned(),
            content: serde_json::json!({"room_id": "!r:here", "typing": true}),
            coalesce_key: Some("typing !r:here @a:here".to_owned()),
        };
        let bytes = serde_json::to_vec(&vec![edu.clone()]).unwrap();
        let back: Vec<ForwardedEdu> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, vec![edu]);
    }
}
