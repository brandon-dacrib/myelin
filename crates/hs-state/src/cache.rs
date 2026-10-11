//! [`LruCache`]: the small, bounded, least-recently-used map the production state store keeps
//! its working set in (`crate::kv_store`).
//!
//! Until 2026-10-10 `KvStateStore` held a record of every event of its room in memory for the
//! store's lifetime, rebuilt by replaying the room on every open
//! (`docs/rfcs/0025-a-room-load-that-does-not-replay-its-history.md`). Everything it held is
//! now durable, and what stays resident is what was touched recently, bounded by a capacity
//! per kind of record. A cache here is only ever filled from committed reads (or after the
//! store's own commit), never from a write still inside an open transaction, so a conflicted
//! and retried transaction can never leave a stale entry behind: every row cached is immutable
//! once written (a state root, a chain position, an interned key, an event record).

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

/// A bounded least-recently-used map. `capacity == 0` caches nothing.
#[derive(Debug)]
pub struct LruCache<K, V> {
    entries: HashMap<K, (V, u64)>,
    order: BTreeMap<u64, K>,
    tick: u64,
    capacity: usize,
}

impl<K: Hash + Eq + Clone, V> LruCache<K, V> {
    /// An empty cache holding at most `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            capacity,
        }
    }

    /// How many entries are resident.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is resident.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The capacity this cache was created with.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The value for `key`, marking it as the most recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let tick = self.next_tick();
        let (value, last) = self.entries.get_mut(key)?;
        self.order.remove(last);
        *last = tick;
        self.order.insert(tick, key.clone());
        Some(value)
    }

    /// The value for `key` without touching its recency.
    #[must_use]
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|(value, _)| value)
    }

    /// Whether `key` is resident, without touching its recency.
    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Inserts or replaces `key`, evicting the least recently used entry if over capacity.
    /// Returns how many entries were evicted (0 or 1).
    pub fn insert(&mut self, key: K, value: V) -> usize {
        if self.capacity == 0 {
            return 0;
        }
        let tick = self.next_tick();
        if let Some((_, last)) = self.entries.get(&key) {
            self.order.remove(last);
        }
        self.order.insert(tick, key.clone());
        self.entries.insert(key, (value, tick));
        let mut evicted = 0;
        while self.entries.len() > self.capacity {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.entries.remove(&oldest);
            evicted += 1;
        }
        evicted
    }

    /// Removes `key`, returning its value if it was resident.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let (value, last) = self.entries.remove(key)?;
        self.order.remove(&last);
        Some(value)
    }

    /// Drops every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_the_least_recently_used_entry() {
        let mut cache = LruCache::new(2);
        cache.insert(1, "a");
        cache.insert(2, "b");
        assert_eq!(cache.get(&1), Some(&"a"));
        // 2 is now the least recently used.
        assert_eq!(cache.insert(3, "c"), 1);
        assert!(cache.contains(&1));
        assert!(!cache.contains(&2));
        assert!(cache.contains(&3));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn replacing_a_key_keeps_one_entry() {
        let mut cache = LruCache::new(2);
        cache.insert(1, "a");
        cache.insert(1, "b");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.peek(&1), Some(&"b"));
        assert_eq!(cache.remove(&1), Some("b"));
        assert!(cache.is_empty());
    }

    #[test]
    fn a_zero_capacity_cache_holds_nothing() {
        let mut cache = LruCache::new(0);
        cache.insert(1, "a");
        assert!(cache.is_empty());
        assert_eq!(cache.get(&1), None);
    }
}
