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
//! [`RuleCache`] is the fix: an in-memory `user_id -> Arc<Ruleset>` map. A first read loads
//! and caches; every later lookup for that user is a `HashMap` hit plus an `Arc` clone until
//! [`RuleCache::invalidate`] is called. `crate::rulesets::CachedRulesetStore` is the store
//! wrapper that calls `invalidate` on every write, so a user's own rule changes are visible on
//! their very next evaluation. On a single node there is no TTL and nothing to tune: the only
//! way a cached entry goes stale there is a write this same process just made.
//!
//! # Across replicas
//!
//! In a cluster, a user's push rules can be written on any replica (wherever their client's
//! request lands), and read on others: the room owner evaluating events for them, and the
//! replica serving their `/sync` or `GET /pushrules`. Each entry therefore carries the
//! store's change-seq it was read at (`crate::rulesets::RulesetStore::changed_seq`), and
//! `crate::rulesets::CachedRulesetStore` keeps it right three ways:
//!
//! - a write anywhere tells the other replicas, which drop their entry
//!   (`crate::rulesets::RulesetChangeFeed`, carried over the cluster mesh by `hs-cli`);
//! - `/sync` and `GET /pushrules` compare the entry's seq with the store's on every read (one
//!   small point read), so what a client is shown is never stale;
//! - the evaluation path re-checks an entry's seq once it is older than the store's
//!   revalidation interval (set in a cluster only), which bounds how long a lost mesh message
//!   can leave a stale entry in use.
//!
//! A single node needs none of this: every write goes through its own cache.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};
use std::time::Instant;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;

use crate::ruleset::Ruleset;
use ruma::OwnedUserId;

/// One cached ruleset: the ruleset, the store's change-seq it was read at (`None` when put in
/// without one), and when that seq was last confirmed against the store.
#[derive(Debug, Clone)]
pub struct CachedRules {
    /// The ruleset.
    pub ruleset: Arc<Ruleset>,
    /// The user's change-seq when it was read, if known.
    pub seq: Option<u64>,
    /// When `seq` was read or last found current.
    pub checked: Instant,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct InvalidationLabels {
    source: &'static str,
}

static INVALIDATIONS: LazyLock<Family<InvalidationLabels, Counter>> =
    LazyLock::new(Family::default);

/// Registers `hs_push_rule_cache_invalidations_total{source}`: cached rulesets dropped because
/// this process wrote them (`local`), another replica said it did (`peer`), or a check against
/// the store found them behind (`stale`; in a cluster, a change whose message did not arrive).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_push_rule_cache_invalidations",
        "Cached push rulesets dropped, by why: written here (local), written on another replica \
         (peer), or found behind the store (stale)",
        INVALIDATIONS.clone(),
    );
}

/// Counts one invalidation from `source` (`local`, `peer` or `stale`).
pub(crate) fn count_invalidation(source: &'static str) {
    INVALIDATIONS
        .get_or_create(&InvalidationLabels { source })
        .inc();
}

/// An in-memory cache of compiled (i.e. already-deserialized) per-user rulesets.
///
/// A reader that missed the cache, read the store and then inserts what it read must not put
/// back a ruleset a write replaced in the meantime: the stale entry would then be served until
/// the user's next write. [`RuleCache::generation`] and [`RuleCache::insert_if_unchanged`] are
/// how it avoids that: every [`RuleCache::invalidate`] moves the generation on, and an insert
/// taken at an older generation is dropped.
#[derive(Debug, Default)]
pub struct RuleCache {
    entries: RwLock<HashMap<OwnedUserId, CachedRules>>,
    /// Moved on by every [`RuleCache::invalidate`], under the `entries` write lock.
    generation: AtomicU64,
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
        self.entry(user_id).map(|e| e.ruleset)
    }

    /// The cached entry for `user_id`, with the change-seq it was read at.
    #[must_use]
    pub fn entry(&self, user_id: &ruma::UserId) -> Option<CachedRules> {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(user_id)
            .cloned()
    }

    /// Caches `ruleset` for `user_id`, replacing whatever was cached before, with no change-seq
    /// (so it is re-read the first time one is asked for).
    pub fn insert(&self, user_id: OwnedUserId, ruleset: Arc<Ruleset>) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                user_id,
                CachedRules {
                    ruleset,
                    seq: None,
                    checked: Instant::now(),
                },
            );
    }

    /// Records that `user_id`'s entry, read at `seq`, was just found current, if it still is
    /// the entry read at `seq`.
    pub fn confirm(&self, user_id: &ruma::UserId, seq: u64) {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = entries.get_mut(user_id)
            && entry.seq == Some(seq)
        {
            entry.checked = Instant::now();
        }
    }

    /// The cache's generation: read it before reading a ruleset from the store, and pass it to
    /// [`RuleCache::insert_if_unchanged`] with what was read.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Caches `ruleset`, read at change-seq `seq`, for `user_id` only if nothing has been
    /// invalidated since `generation` was read ([`RuleCache::generation`]); returns whether it
    /// was cached. A ruleset read from the store before a concurrent write landed is then not
    /// cached over that write. (Any user's invalidation counts: writes are rare, and the next
    /// read caches it.)
    pub fn insert_if_unchanged(
        &self,
        user_id: OwnedUserId,
        ruleset: Arc<Ruleset>,
        seq: Option<u64>,
        generation: u64,
    ) -> bool {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if self.generation.load(Ordering::Acquire) != generation {
            return false;
        }
        entries.insert(
            user_id,
            CachedRules {
                ruleset,
                seq,
                checked: Instant::now(),
            },
        );
        true
    }

    /// Drops the cached entry for `user_id`, if any, and moves the generation on. Called by
    /// `crate::rulesets::CachedRulesetStore` after every write to that user's ruleset.
    pub fn invalidate(&self, user_id: &ruma::UserId) {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        entries.remove(user_id);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// The number of users currently cached, for tests and metrics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
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

    #[test]
    fn a_read_from_before_an_invalidation_is_not_cached() {
        let cache = RuleCache::new();
        let alice = user_id!("@alice:example.org");
        let generation = cache.generation();
        // A write lands between the reader's store read and its insert.
        cache.invalidate(alice);
        let stale = Arc::new(Ruleset::server_default(alice));
        assert!(!cache.insert_if_unchanged(alice.to_owned(), stale.clone(), Some(1), generation));
        assert!(cache.peek(alice).is_none(), "the stale read was cached");
        // Read again after the write: cached.
        let generation = cache.generation();
        assert!(cache.insert_if_unchanged(alice.to_owned(), stale, Some(1), generation));
        assert_eq!(cache.entry(alice).and_then(|e| e.seq), Some(1));
    }
}
