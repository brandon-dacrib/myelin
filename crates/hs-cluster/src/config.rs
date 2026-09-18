//! Cluster configuration. Mirrors the `cluster` config section track 13 owns
//! (`docs/rfcs/0001-cluster-ownership.md` section 17); this crate does not parse YAML itself, it
//! just defines the typed shape and sensible defaults so 13 has something concrete to bind to.

use std::time::Duration;

use crate::mesh::auth::AuthMode;
use crate::types::{ReplicaId, ShardLayout};

/// Top-level cluster configuration for one replica.
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// This replica's identity.
    pub me: ReplicaId,
    /// `host:port` the mesh listener advertises to peers (written into the replica's registry
    /// row).
    pub mesh_advertise_addr: String,
    /// Topology zone, when known (`topology.kubernetes.io/zone`).
    pub zone: Option<String>,
    /// Binary version string, for rolling-upgrade decisions.
    pub version: String,
    /// The shard layout, fixed at cluster creation (RFC 0001 section 3).
    pub layout: ShardLayout,
    /// How often a replica rewrites its heartbeat row. Default 1 s.
    pub heartbeat_interval: Duration,
    /// How long since an *observed* heartbeat change before a peer is judged dead. Must be at
    /// least `2 * heartbeat_interval`. Default 3 s.
    pub lease_ttl: Duration,
    /// Mesh transport and authentication settings.
    pub mesh: MeshConfig,
    /// Graceful handoff settings.
    pub handoff: HandoffConfig,
}

impl ClusterConfig {
    /// Defaults for a clustered replica, given only its identity and layout; every timing and
    /// mesh knob takes the RFC's documented default.
    #[must_use]
    pub fn new(me: ReplicaId, mesh_advertise_addr: impl Into<String>, layout: ShardLayout) -> Self {
        Self {
            me,
            mesh_advertise_addr: mesh_advertise_addr.into(),
            zone: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            layout,
            heartbeat_interval: Duration::from_secs(1),
            lease_ttl: Duration::from_secs(3),
            mesh: MeshConfig::default(),
            handoff: HandoffConfig::default(),
        }
    }

    /// Validates the timing relationship the failure detector depends on.
    ///
    /// # Errors
    /// Returns a message if `lease_ttl` is too short relative to `heartbeat_interval`.
    pub fn validate(&self) -> Result<(), String> {
        if self.lease_ttl < self.heartbeat_interval * 2 {
            return Err(format!(
                "cluster.lease_ttl ({:?}) must be at least twice cluster.heartbeat_interval ({:?})",
                self.lease_ttl, self.heartbeat_interval
            ));
        }
        self.layout.validate()
    }
}

/// Mesh transport and backpressure settings (RFC 0001 sections 8, 9 and 11).
#[derive(Debug, Clone)]
pub struct MeshConfig {
    /// `host:port` to bind the mesh HTTP/2 listener on.
    pub listen_addr: String,
    /// Authentication mode.
    pub auth: AuthMode,
    /// Bounded in-flight forwards per peer before the sender fails fast with `503`.
    pub max_in_flight_per_peer: usize,
    /// Cap on concurrent outbound forwards for one fan-out request (an appservice or federation
    /// send split across many owners).
    pub max_fan_out: usize,
    /// Forwards are dropped rather than looped past this many hops.
    pub max_hops: u32,
    /// Maximum retry attempts at the forwarding edge, within the request's deadline.
    pub max_attempts: u32,
    /// Base backoff between forward retries.
    pub retry_base_backoff: Duration,
    /// Default per-request deadline when the caller does not set one explicitly.
    pub default_deadline: Duration,
    /// How long a completed reply is kept in the per-shard idempotency cache.
    pub idempotency_ttl: Duration,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:8449".into(),
            auth: AuthMode::SharedSecret {
                secret: "dev-only-shared-secret".into(),
            },
            max_in_flight_per_peer: 1024,
            max_fan_out: 64,
            max_hops: 3,
            max_attempts: 4,
            retry_base_backoff: Duration::from_millis(10),
            default_deadline: Duration::from_secs(10),
            idempotency_ttl: Duration::from_secs(60),
        }
    }
}

/// Graceful handoff / drain settings (RFC 0001 section 10).
#[derive(Debug, Clone, Copy)]
pub struct HandoffConfig {
    /// How many shards are released and handed off concurrently during drain.
    pub parallelism: usize,
    /// Safety margin subtracted from the caller-supplied deadline before drain gives up waiting
    /// for every shard to show a new owner.
    pub safety_margin: Duration,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            parallelism: 32,
            safety_margin: Duration::from_secs(2),
        }
    }
}
