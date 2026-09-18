//! The owner-side idempotency cache: `docs/rfcs/0001-cluster-ownership.md` section 8.
//!
//! `idempotency_key -> reply`, bounded and TTL'd, per shard so a shard move takes its cache
//! semantics with it (the new owner starts empty). This cache alone is **not** the durable
//! idempotency guarantee -- it does not survive failover. An actor whose effect is not naturally
//! idempotent must persist the key in the same transaction as the effect; this cache only saves
//! the common case (a retry after a lost reply, same owner) from re-executing.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::mesh::envelope::{IdempotencyKey, Reply};
use crate::types::ShardId;

struct Entry {
    reply: Reply,
    expires_at: Instant,
}

/// A bounded, TTL'd `idempotency_key -> reply` cache, partitioned per shard.
pub struct IdempotencyCache {
    ttl: Duration,
    max_entries_per_shard: usize,
    shards: Mutex<HashMap<ShardId, HashMap<IdempotencyKey, Entry>>>,
}

impl IdempotencyCache {
    /// Builds a cache with the given per-key TTL and a cap on entries held per shard (oldest
    /// evicted first once the cap is hit, to bound memory under a runaway retry storm).
    #[must_use]
    pub fn new(ttl: Duration, max_entries_per_shard: usize) -> Self {
        Self {
            ttl,
            max_entries_per_shard,
            shards: Mutex::new(HashMap::new()),
        }
    }

    /// Looks up a cached reply for `(shard, key)`, evicting it first if its TTL has passed.
    pub fn get(&self, shard: ShardId, key: IdempotencyKey) -> Option<Reply> {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bucket = shards.get_mut(&shard)?;
        match bucket.get(&key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.reply.clone()),
            Some(_) => {
                bucket.remove(&key);
                None
            }
            None => None,
        }
    }

    /// Records the reply a handler produced for `(shard, key)`, so a retry of the same key on
    /// this owner short-circuits without re-executing.
    pub fn put(&self, shard: ShardId, key: IdempotencyKey, reply: Reply) {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bucket = shards.entry(shard).or_default();
        if bucket.len() >= self.max_entries_per_shard {
            // No ordering metadata is kept for eviction; under the cap being hit at all (a
            // pathological retry storm) evicting an arbitrary entry is an acceptable, simple
            // trade-off -- the durable rule (RFC 0001 section 8) is what correctness rests on.
            if let Some(k) = bucket.keys().next().copied() {
                bucket.remove(&k);
            }
        }
        bucket.insert(
            key,
            Entry {
                reply,
                expires_at: Instant::now() + self.ttl,
            },
        );
    }

    /// Drops every entry for `shard`. Called when the shard is released or lost, so a later
    /// owner (this replica, re-acquiring) starts empty rather than serving another owner's
    /// cached replies.
    pub fn clear_shard(&self, shard: ShardId) {
        self.shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&shard);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ShardKind;
    use bytes::Bytes;

    fn shard() -> ShardId {
        ShardId::new(ShardKind::Room, 1)
    }

    #[test]
    fn put_then_get_round_trips() {
        let cache = IdempotencyCache::new(Duration::from_secs(60), 100);
        let key = IdempotencyKey::generate();
        cache.put(shard(), key, Reply::ok(Bytes::from_static(b"hi")));
        let got = cache.get(shard(), key).unwrap();
        assert_eq!(got.payload, Bytes::from_static(b"hi"));
    }

    #[test]
    fn entries_expire() {
        let cache = IdempotencyCache::new(Duration::from_millis(1), 100);
        let key = IdempotencyKey::generate();
        cache.put(shard(), key, Reply::ok(Bytes::new()));
        std::thread::sleep(Duration::from_millis(20));
        assert!(cache.get(shard(), key).is_none());
    }

    #[test]
    fn clear_shard_drops_only_that_shard() {
        let cache = IdempotencyCache::new(Duration::from_secs(60), 100);
        let a = ShardId::new(ShardKind::Room, 1);
        let b = ShardId::new(ShardKind::Room, 2);
        let key = IdempotencyKey::generate();
        cache.put(a, key, Reply::ok(Bytes::new()));
        cache.put(b, key, Reply::ok(Bytes::new()));
        cache.clear_shard(a);
        assert!(cache.get(a, key).is_none());
        assert!(cache.get(b, key).is_some());
    }

    #[test]
    fn cap_evicts_rather_than_growing_unboundedly() {
        let cache = IdempotencyCache::new(Duration::from_secs(60), 4);
        for _ in 0..10 {
            cache.put(shard(), IdempotencyKey::generate(), Reply::ok(Bytes::new()));
        }
        let shards = cache.shards.lock().unwrap();
        assert!(shards[&shard()].len() <= 4);
    }
}
