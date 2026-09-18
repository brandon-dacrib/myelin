//! Per-user compiled rule sets with invalidation, so evaluating one event against many
//! recipients does not re-parse rules per user (the brief's deliverable 2 — this is on the hot
//! path of every message sent in a busy room).
//!
//! # What "compiled" means here
//!
//! `ruma::push::Ruleset` is already the compiled representation: an `IndexSet` per rule kind
//! (O(1) `rule_id` lookup, insertion-order iteration that already matches the spec's priority
//! order — see `crate::engine`'s doc comment). There is no second, bespoke compilation step to
//! build on top of it without duplicating work Ruma already did well, which
//! `docs/decisions/0007-build-less-reuse-more.md` rules out. What *is* genuinely expensive per
//! event, and genuinely ours to fix, is fetching a user's ruleset from the store and
//! deserializing it from JSON on every single recipient of every single event in a room — for a
//! busy room with a thousand joined members, that is a thousand store reads and a thousand JSON
//! parses per message, all for rules that change on the order of "a user edits their
//! notification settings", not "a user receives a message".
//!
//! [`RuleCache`] is the fix: an in-memory `user_id -> Arc<Ruleset>` map. [`RuleCache::get`] loads
//! and caches on first use; every subsequent lookup for that user is a `HashMap` hit plus an
//! `Arc` clone until [`RuleCache::invalidate`] is called. `crate::rulesets::CachedRulesetStore`
//! is the store wrapper that calls `invalidate` on every write, so a user's own rule changes are
//! visible on their very next evaluation — there is no TTL and nothing to tune, because the only
//! way a cached entry goes stale in this process is a write this same process just made.
//!
//! Invalidation only covers writes made through this process. A clustered deployment where a
//! different replica's `hs-user` session actor writes a user's push rules (moving them, in
//! `PLAN.md`'s terms, off *this* replica's cache) needs a cross-replica invalidation signal —
//! `docs/status/10-push.md`'s "Interfaces needed" records this as owed to track 03's mesh once
//! that pub/sub primitive exists; until then, [`RuleCache`] is correct for a single replica
//! (including `--single-node`) and merely slow-to-notice a same-user write from elsewhere in a
//! multi-replica deployment (bounded by that user's next natural cache eviction — there isn't
//! one yet, so today it means "until the process restarts", a known, recorded gap, not a silent
//! one).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use ruma::OwnedUserId;
use ruma::push::Ruleset;

/// An in-memory cache of compiled (i.e. already-deserialized) per-user rulesets.
#[derive(Debug, Default)]
pub struct RuleCache {
    entries: RwLock<HashMap<OwnedUserId, Arc<Ruleset>>>,
}

impl RuleCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the cached ruleset for `user_id`, or `None` if nothing is cached (a cold cache, or
    /// after [`RuleCache::invalidate`]).
    #[must_use]
    pub fn peek(&self, user_id: &ruma::UserId) -> Option<Arc<Ruleset>> {
        self.entries.read().unwrap().get(user_id).cloned()
    }

    /// Caches `ruleset` for `user_id`, replacing whatever was cached before.
    pub fn insert(&self, user_id: OwnedUserId, ruleset: Arc<Ruleset>) {
        self.entries.write().unwrap().insert(user_id, ruleset);
    }

    /// Drops the cached entry for `user_id`, if any. Called by `crate::rulesets::CachedRulesetStore`
    /// after every write to that user's ruleset.
    pub fn invalidate(&self, user_id: &ruma::UserId) {
        self.entries.write().unwrap().remove(user_id);
    }

    /// The number of users currently cached, for tests and metrics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    /// True if nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    #[test]
    fn peek_misses_on_a_cold_cache() {
        let cache = RuleCache::new();
        assert!(cache.peek(user_id!("@alice:example.org")).is_none());
    }

    #[test]
    fn insert_then_peek_hits() {
        let cache = RuleCache::new();
        let alice = user_id!("@alice:example.org");
        let ruleset = Arc::new(Ruleset::server_default(alice));
        cache.insert(alice.to_owned(), ruleset.clone());
        let hit = cache.peek(alice).expect("should be cached");
        assert!(Arc::ptr_eq(&hit, &ruleset));
    }

    #[test]
    fn invalidate_evicts_only_the_named_user() {
        let cache = RuleCache::new();
        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        cache.insert(alice.to_owned(), Arc::new(Ruleset::server_default(alice)));
        cache.insert(bob.to_owned(), Arc::new(Ruleset::server_default(bob)));
        cache.invalidate(alice);
        assert!(cache.peek(alice).is_none());
        assert!(cache.peek(bob).is_some());
        assert_eq!(cache.len(), 1);
    }
}
