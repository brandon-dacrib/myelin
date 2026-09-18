//! Error types for `hs-cluster`.

use crate::types::{Epoch, ShardId};

/// Errors from the cluster store layer (built directly on [`hs_kv::KvBackend`]; see
/// `docs/rfcs/0001-cluster-ownership.md` section 6).
#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    /// The underlying `hs-kv` backend failed.
    #[error("store error: {0}")]
    Store(#[from] hs_kv::KvError),

    /// A stored record could not be decoded. This should never happen for records this crate
    /// wrote itself; it indicates a schema mismatch or corruption.
    #[error("failed to decode {what}: {source}")]
    Decode {
        /// What was being decoded (`"ReplicaRecord"`, `"ShardRecord"`, `"ShardLayout"`).
        what: &'static str,
        /// The underlying decode error.
        #[source]
        source: serde_json::Error,
    },

    /// The replica booted with a shard layout that differs from the one recorded in the store at
    /// cluster creation. The layout is immutable after creation (RFC 0001 section 3).
    #[error(
        "shard layout mismatch: this replica requested {requested:?} but the store has {recorded:?}"
    )]
    LayoutMismatch {
        /// What this replica asked for.
        requested: crate::types::ShardLayout,
        /// What is recorded.
        recorded: crate::types::ShardLayout,
    },
}

/// A stale-owner write was rejected because the shard's fencing epoch had moved on. See
/// `docs/rfcs/0001-cluster-ownership.md` section 6: this is the mandatory check every owner
/// transaction performs, built on `hs-kv`'s serializable read-set validation.
#[derive(Debug, thiserror::Error)]
pub enum FenceError {
    /// The shard's current epoch no longer matches the one this fence was issued for: another
    /// replica has since acquired the shard.
    #[error("shard {shard} was fenced: this fence holds epoch {held}, current is {current:?}")]
    Fenced {
        /// The shard.
        shard: ShardId,
        /// The epoch this fence was issued for.
        held: Epoch,
        /// The epoch currently recorded, or `None` if the shard was released.
        current: Option<Epoch>,
    },

    /// The epoch read itself failed.
    #[error("fence check: store error: {0}")]
    Store(#[source] hs_kv::KvError),
}

/// Mesh authentication failures.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// No credentials were presented.
    #[error("no mesh credentials presented")]
    Missing,
    /// Credentials were presented but rejected.
    #[error("mesh credentials rejected: {0}")]
    Rejected(String),
}

/// Errors forwarding a request to a shard's owner over the mesh.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    /// No replica currently owns the shard (e.g. mid-failover with no live candidates, or
    /// single-node mode where `forward` should never be reached).
    #[error("shard {0} has no known owner")]
    NoOwner(ShardId),

    /// This process is not part of a cluster (single-node mode): `forward` should never be
    /// called, because `is_mine` is always true.
    #[error("not clustered: single-node mode has no mesh")]
    NotClustered,

    /// The request bounced between replicas more than `max_hops` times without landing on a
    /// stable owner.
    #[error("forward exceeded {max_hops} hops for shard {shard}")]
    TooManyHops {
        /// The shard being forwarded for.
        shard: ShardId,
        /// The configured limit.
        max_hops: u32,
    },

    /// Every retry attempt was exhausted within the request's deadline.
    #[error("forward to shard {shard} exhausted {attempts} attempts")]
    RetriesExhausted {
        /// The shard being forwarded for.
        shard: ShardId,
        /// Attempts made.
        attempts: u32,
    },

    /// The request's deadline passed before it could be delivered.
    #[error("forward to shard {0} missed its deadline")]
    DeadlineExceeded(ShardId),

    /// A transport-level failure talking to a peer.
    #[error("mesh transport error: {0}")]
    Transport(String),

    /// The peer rejected our credentials.
    #[error("mesh auth error: {0}")]
    Auth(#[from] AuthError),
}

/// Errors from the graceful-handoff / drain sequence.
#[derive(Debug, thiserror::Error)]
pub enum DrainError {
    /// The store could not be reached to release a shard.
    #[error("drain: store error releasing {shard}: {source}")]
    Store {
        /// The shard being released.
        shard: ShardId,
        /// The underlying error.
        #[source]
        source: ClusterError,
    },
}
