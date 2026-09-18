//! Identity types shared by every part of the cluster: replicas, generations,
//! shards, epochs and the store records that describe them.
//!
//! See `docs/rfcs/0001-cluster-ownership.md` sections 2 to 4.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::hash::stable_hash64;

/// The name of one `hs serve` process. The pod name on Kubernetes, the host
/// name otherwise. Stable across restarts of the same pod; a restart gets a
/// new [`Generation`].
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReplicaId(String);

impl ReplicaId {
    /// Wraps a replica name.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ReplicaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ReplicaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReplicaId({})", self.0)
    }
}

impl From<&str> for ReplicaId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// The incarnation of a replica: strictly increasing across restarts of the
/// same [`ReplicaId`]. Rows written by an older generation are ignored.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Generation(pub u64);

impl Generation {
    /// A generation for a fresh process: the wall clock in milliseconds, or
    /// one more than the previous generation if the clock went backwards.
    pub fn fresh(previous: Option<Generation>) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let floor = previous.map(|g| g.0 + 1).unwrap_or(0);
        Self(now_ms.max(floor))
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The fencing token of a shard: incremented on every acquisition.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Epoch(pub u64);

impl Epoch {
    /// The epoch after this one.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The kinds of shard. Each kind is an independent index space.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShardKind {
    /// Room actors and everything a room owns.
    Room,
    /// User session actors.
    User,
    /// Outbound federation queues, by destination server.
    Federation,
    /// Outbound appservice transaction queues, by appservice id.
    Appservice,
    /// The single global shard: singletons and background jobs.
    Global,
}

impl ShardKind {
    /// Every kind, in a fixed order.
    pub const ALL: [ShardKind; 5] = [
        ShardKind::Room,
        ShardKind::User,
        ShardKind::Federation,
        ShardKind::Appservice,
        ShardKind::Global,
    ];

    /// The short name used in keys and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            ShardKind::Room => "room",
            ShardKind::User => "user",
            ShardKind::Federation => "federation",
            ShardKind::Appservice => "appservice",
            ShardKind::Global => "global",
        }
    }

    /// Parses the short name.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl fmt::Display for ShardKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The unit of ownership.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ShardId {
    /// The kind of shard.
    pub kind: ShardKind,
    /// Index within the kind, below the layout's count for that kind.
    pub index: u32,
}

impl ShardId {
    /// Builds a shard id.
    pub const fn new(kind: ShardKind, index: u32) -> Self {
        Self { kind, index }
    }

    /// The single global shard.
    pub const GLOBAL: ShardId = ShardId::new(ShardKind::Global, 0);

    /// The stable byte form used as hash input: `kind/index`.
    pub fn key_bytes(&self) -> Vec<u8> {
        format!("{}/{}", self.kind.as_str(), self.index).into_bytes()
    }

    /// Parses `kind/index`.
    pub fn parse(s: &str) -> Option<Self> {
        let (kind, index) = s.split_once('/')?;
        Some(Self {
            kind: ShardKind::parse(kind)?,
            index: index.parse().ok()?,
        })
    }
}

impl fmt::Display for ShardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.kind.as_str(), self.index)
    }
}

impl fmt::Debug for ShardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ShardId({}/{})", self.kind.as_str(), self.index)
    }
}

/// The number of shards per kind. Fixed at cluster creation, recorded in the
/// store, immutable afterwards (RFC 0001 section 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ShardLayout {
    /// Room shards.
    pub rooms: u32,
    /// User shards.
    pub users: u32,
    /// Federation sender shards.
    pub federation: u32,
    /// Appservice sender shards.
    pub appservice: u32,
}

impl Default for ShardLayout {
    fn default() -> Self {
        Self {
            rooms: 256,
            users: 256,
            federation: 64,
            appservice: 64,
        }
    }
}

impl ShardLayout {
    /// A tiny layout for tests.
    pub const fn small(n: u32) -> Self {
        Self {
            rooms: n,
            users: n,
            federation: n,
            appservice: n,
        }
    }

    /// Shards of one kind. The global kind always has one.
    pub fn count(&self, kind: ShardKind) -> u32 {
        match kind {
            ShardKind::Room => self.rooms,
            ShardKind::User => self.users,
            ShardKind::Federation => self.federation,
            ShardKind::Appservice => self.appservice,
            ShardKind::Global => 1,
        }
    }

    /// Total number of shards across kinds.
    pub fn total(&self) -> u32 {
        ShardKind::ALL.iter().map(|k| self.count(*k)).sum()
    }

    /// Every shard in a fixed order.
    pub fn all_shards(&self) -> impl Iterator<Item = ShardId> + '_ {
        ShardKind::ALL
            .into_iter()
            .flat_map(move |kind| (0..self.count(kind)).map(move |index| ShardId { kind, index }))
    }

    /// Whether every count is at least one.
    pub fn validate(&self) -> Result<(), String> {
        for kind in ShardKind::ALL {
            if self.count(kind) == 0 {
                return Err(format!("shard layout: {kind} count must be at least 1"));
            }
        }
        Ok(())
    }

    fn shard_for(&self, kind: ShardKind, id: &[u8]) -> ShardId {
        let n = self.count(kind).max(1) as u64;
        ShardId {
            kind,
            index: (stable_hash64(&[kind.as_str().as_bytes(), b":", id]) % n) as u32,
        }
    }

    /// The room shard of a room id.
    pub fn room_shard(&self, room_id: &str) -> ShardId {
        self.shard_for(ShardKind::Room, room_id.as_bytes())
    }

    /// The user shard of a user id.
    pub fn user_shard(&self, user_id: &str) -> ShardId {
        self.shard_for(ShardKind::User, user_id.as_bytes())
    }

    /// The federation sender shard of a destination server name.
    pub fn federation_shard(&self, destination: &str) -> ShardId {
        self.shard_for(ShardKind::Federation, destination.as_bytes())
    }

    /// The appservice sender shard of an appservice id.
    pub fn appservice_shard(&self, appservice_id: &str) -> ShardId {
        self.shard_for(ShardKind::Appservice, appservice_id.as_bytes())
    }
}

/// Where a replica is in its life.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaState {
    /// Started, heartbeating, not yet converged.
    Joining,
    /// Serving.
    Active,
    /// Shutting down: excluded from hashing, releasing its shards.
    Draining,
    /// Gone; the row is about to be removed.
    Left,
}

impl ReplicaState {
    /// Whether the replica takes part in rendezvous hashing.
    pub fn is_hashable(self) -> bool {
        matches!(self, ReplicaState::Joining | ReplicaState::Active)
    }
}

/// A replica's registry row (RFC 0001 section 4).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ReplicaRecord {
    /// The replica.
    pub id: ReplicaId,
    /// Its incarnation.
    pub generation: Generation,
    /// `host:port` of its mesh listener.
    pub mesh_addr: String,
    /// Topology zone, when known.
    #[serde(default)]
    pub zone: Option<String>,
    /// Binary version, for rolling-upgrade decisions.
    #[serde(default)]
    pub version: String,
    /// Lifecycle state.
    pub state: ReplicaState,
    /// Incremented on every heartbeat; the observed-change liveness signal.
    pub heartbeat_seq: u64,
    /// Wall clock of the last heartbeat, a hint for operators only.
    pub heartbeat_unix_ms: u64,
}

/// A shard's ownership row (RFC 0001 section 6).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ShardRecord {
    /// The fencing token.
    pub epoch: Epoch,
    /// The owner and its generation, or `None` when released.
    pub owner: Option<(ReplicaId, Generation)>,
}

impl ShardRecord {
    /// A never-owned shard.
    pub const fn initial() -> Self {
        Self {
            epoch: Epoch(0),
            owner: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_id_round_trips_through_text() {
        for s in ShardLayout::small(3).all_shards() {
            assert_eq!(ShardId::parse(&s.to_string()), Some(s));
        }
        assert_eq!(ShardId::parse("nope/1"), None);
        assert_eq!(ShardId::parse("room/x"), None);
    }

    #[test]
    fn layout_counts_and_mapping_are_stable() {
        let layout = ShardLayout::default();
        assert_eq!(layout.total(), 256 + 256 + 64 + 64 + 1);
        let a = layout.room_shard("!abc:example.org");
        assert_eq!(a, layout.room_shard("!abc:example.org"));
        assert_eq!(a.kind, ShardKind::Room);
        assert!(a.index < 256);
        // A room and a user with the same string map into different kinds.
        assert_eq!(layout.user_shard("!abc:example.org").kind, ShardKind::User);
        // Pinned values: the mapping must never change across versions.
        assert_eq!(layout.room_shard("!room:example.org").index, 45);
        assert_eq!(layout.user_shard("@alice:example.org").index, 44);
    }

    #[test]
    fn generation_is_monotonic() {
        let g1 = Generation::fresh(None);
        let g2 = Generation::fresh(Some(Generation(u64::MAX - 1)));
        assert_eq!(g2, Generation(u64::MAX));
        assert!(g1.0 > 1_600_000_000_000);
    }

    #[test]
    fn layout_validation() {
        assert!(ShardLayout::default().validate().is_ok());
        assert!(ShardLayout::small(0).validate().is_err());
    }
}
