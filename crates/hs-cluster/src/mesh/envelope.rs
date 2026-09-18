//! The forwarding envelope: `docs/rfcs/0001-cluster-ownership.md` section 8.
//!
//! Metadata travels in HTTP headers, the payload as the body, so the owner never re-serialises a
//! forwarded request. `POST /mesh/v1/forward` is the one route this envelope is carried over.

use bytes::Bytes;

use crate::types::{Generation, ReplicaId, ShardId};

/// The authenticated requester context from track 07's auth middleware (user, device,
/// appservice assertion, admin flag). Track 07 has not frozen `RequesterContext` yet (RFC 0001
/// section 17), so the mesh carries it as opaque JSON, trusted because the mesh transport itself
/// is authenticated (section 11) -- the client-facing edge is the only place that authenticates
/// end users.
pub type RequesterContext = serde_json::Value;

/// A 128-bit idempotency key, generated once per client request at the edge and reused on every
/// retry of that request (including retries that land on a different owner after a `421`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IdempotencyKey(pub u128);

impl IdempotencyKey {
    /// Generates a fresh random key.
    #[must_use]
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        Self(u128::from_le_bytes(bytes))
    }
}

impl std::fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl std::str::FromStr for IdempotencyKey {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        u128::from_str_radix(s, 16).map(Self)
    }
}

/// A forwarded request. See the module docs and RFC 0001 section 8 for the field-by-field
/// rationale.
#[derive(Debug, Clone)]
pub struct Envelope {
    /// The target shard.
    pub shard: ShardId,
    /// Which handler on the owner should process this (`"room.send"`, `"user.sync"`, ...). The
    /// cluster never interprets this string; it is opaque routing for [`ShardHandler`].
    pub route: String,
    /// Reused on every retry of the same client request.
    pub idempotency_key: IdempotencyKey,
    /// The authenticated requester, trusted because the mesh transport is authenticated.
    pub requester: RequesterContext,
    /// Remaining budget, decremented per hop; work whose deadline has passed is dropped rather
    /// than started.
    pub deadline: std::time::Duration,
    /// The replica that originated this request (the edge, or a re-forwarding hop).
    pub origin: ReplicaId,
    /// The origin's generation.
    pub origin_generation: Generation,
    /// Hop count so far; incremented on every forward.
    pub hops: u32,
    /// W3C trace context, so a forwarded request remains one trace.
    pub traceparent: Option<String>,
    /// The opaque request payload.
    pub payload: Bytes,
}

/// A reply to a forwarded request.
#[derive(Debug, Clone)]
pub struct Reply {
    /// HTTP-style status the handler produced (`200` success; application errors are relayed as
    /// their own `4xx`/`5xx`, never retried by the mesh).
    pub status: u16,
    /// The opaque response payload.
    pub payload: Bytes,
}

impl Reply {
    /// A plain `200` reply.
    #[must_use]
    pub fn ok(payload: Bytes) -> Self {
        Self {
            status: 200,
            payload,
        }
    }
}

/// Handles one forwarded request for a shard this replica owns. Implemented by whichever crate
/// owns the actor kind (04's room actor, 05's user session actor, ...); `hs-cluster` only routes
/// to it once ownership and fencing are established.
#[async_trait::async_trait]
pub trait ShardHandler: Send + Sync {
    /// Processes `env`, which the caller has already confirmed targets a shard owned by this
    /// replica at `fence`'s epoch. The handler is responsible for calling
    /// [`crate::fence::Fence::check`] inside every transaction it runs against the shard's data.
    async fn handle(&self, env: Envelope, fence: crate::fence::Fence) -> Reply;
}

pub(crate) mod headers {
    pub const SHARD: &str = "x-hs-shard";
    pub const ROUTE: &str = "x-hs-route";
    pub const IDEMPOTENCY_KEY: &str = "x-hs-idempotency-key";
    pub const REQUESTER: &str = "x-hs-requester";
    pub const DEADLINE_MS: &str = "x-hs-deadline-ms";
    pub const ORIGIN: &str = "x-hs-origin";
    pub const ORIGIN_GENERATION: &str = "x-hs-origin-generation";
    pub const HOPS: &str = "x-hs-hops";
    pub const TRACEPARENT: &str = "traceparent";
    pub const OWNER_HINT: &str = "x-hs-owner-hint";
    pub const RETRY_AFTER_MS: &str = "retry-after-ms";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_round_trips_through_text() {
        let k = IdempotencyKey::generate();
        let s = k.to_string();
        assert_eq!(s.parse::<IdempotencyKey>().unwrap(), k);
    }

    #[test]
    fn generated_keys_are_not_trivially_equal() {
        assert_ne!(IdempotencyKey::generate(), IdempotencyKey::generate());
    }
}
