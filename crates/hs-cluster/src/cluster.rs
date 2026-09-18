//! [`Cluster`]: the top-level facade RFC 0001 section 13 describes -- `start`, `drain`, `ready`,
//! and `single_node`, over either ownership mode. `hs-cli` wires `Cluster::drain` to `SIGTERM`.

use std::sync::Arc;
use std::time::Duration;

use hs_kv::KvBackend;

use crate::config::ClusterConfig;
use crate::error::ClusterError;
use crate::ownership::{DrainReport, Drainable, KvOwnership, Ownership, Readiness, SingleNode};
use crate::types::ReplicaId;

/// The cluster facade a replica process holds for its whole lifetime: the ownership API actors
/// consume, plus the lifecycle operations only `main`/`hs-cli` calls.
#[derive(Clone)]
pub struct Cluster {
    ownership: Arc<dyn Ownership>,
    lifecycle: Arc<dyn Drainable>,
}

impl Cluster {
    /// Single-node mode: `owner_of` is always me, `is_mine` is always true, fencing is a no-op,
    /// there is no registry, heartbeat task or mesh listener (RFC 0001 section 12).
    #[must_use]
    pub fn single_node(me: ReplicaId) -> Self {
        let node = SingleNode::new(me);
        Self {
            ownership: node.clone(),
            lifecycle: node,
        }
    }

    /// Starts a clustered replica: opens the store, confirms the shard layout, and starts the
    /// heartbeat and ownership-convergence background task. The mesh listener is started
    /// separately (see [`crate::mesh::server::MeshServer`]) once the caller has built its
    /// [`crate::mesh::envelope::ShardHandler`]; `Cluster` itself only owns membership and
    /// ownership, matching RFC 0001 section 13's `Ownership` / `Drainable` split.
    ///
    /// # Errors
    /// Returns [`ClusterError`] if the store could not be reached or the shard layout conflicts
    /// with what is already recorded.
    pub async fn start<B: KvBackend>(
        config: ClusterConfig,
        backend: B,
    ) -> Result<(Self, Arc<KvOwnership<B>>), ClusterError> {
        let (manager, _bg_task) = KvOwnership::start(config, backend).await?;
        let cluster = Self {
            ownership: manager.clone(),
            lifecycle: manager.clone(),
        };
        Ok((cluster, manager))
    }

    /// The ownership API: `owner_of`, `is_mine`, `fence`, `subscribe`, `shard_map`.
    #[must_use]
    pub fn ownership(&self) -> &Arc<dyn Ownership> {
        &self.ownership
    }

    /// Whether this replica is ready to serve (RFC 0001 section 7, consumed by track 12's
    /// `/health/ready`).
    #[must_use]
    pub fn ready(&self) -> Readiness {
        self.lifecycle.ready()
    }

    /// Runs the graceful handoff sequence up to `deadline` (RFC 0001 section 10). A no-op that
    /// returns immediately in single-node mode.
    pub async fn drain(&self, deadline: Duration) -> DrainReport {
        self.lifecycle.drain(deadline).await
    }
}

#[cfg(test)]
mod tests {
    use hs_kv::memory::MemoryBackend;

    use super::*;
    use crate::types::{ShardId, ShardKind, ShardLayout};

    #[tokio::test]
    async fn single_node_owns_every_shard_and_drains_instantly() {
        let cluster = Cluster::single_node(ReplicaId::new("hs-solo"));
        let shard = ShardId::new(ShardKind::Room, 3);
        assert!(cluster.ownership().is_mine(shard));
        assert_eq!(cluster.ready(), Readiness::Ready);
        let report = cluster.drain(Duration::from_secs(1)).await;
        assert_eq!(report.handed_off, 0);
        assert_eq!(report.released_unclaimed, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn clustered_start_reports_not_ready_until_a_heartbeat_lands() {
        let backend = MemoryBackend::new();
        let mut config =
            ClusterConfig::new(ReplicaId::new("hs-0"), "127.0.0.1:0", ShardLayout::small(2));
        config.heartbeat_interval = Duration::from_millis(50);
        config.lease_ttl = Duration::from_millis(150);
        let (cluster, _mgr) = Cluster::start(config, backend).await.unwrap();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(60)).await;
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(cluster.ready(), Readiness::Ready);
    }
}
