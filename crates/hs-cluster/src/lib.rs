//! `hs-cluster`: replica registry, leases, rendezvous hashing over virtual shards, fencing
//! epochs, mesh RPC, forwarding, failover and graceful handoff.
//!
//! Owned by track 03 (`docs/workstreams/03-cluster.md`). The design is
//! `docs/rfcs/0001-cluster-ownership.md`; read it first. Everything here builds directly on
//! [`hs_kv::KvBackend`] (track 01) -- there is no separate lease-store abstraction, because
//! `hs-kv`'s serializable transactions already provide the fencing primitive this crate needs
//! (see [`fence`] and `docs/rfcs/0001-cluster-ownership.md` section 6).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cluster;
pub mod config;
pub mod error;
pub mod fence;
pub mod hash;
pub mod mesh;
pub mod metrics;
pub mod ownership;
pub mod store;
pub mod types;

pub use cluster::Cluster;
pub use config::{ClusterConfig, HandoffConfig, MeshConfig};
pub use fence::Fence;
pub use ownership::{DrainReport, Drainable, Ownership, OwnershipEvent, Readiness, ShardMap};
pub use types::{Epoch, Generation, ReplicaId, ShardId, ShardKind, ShardLayout};
