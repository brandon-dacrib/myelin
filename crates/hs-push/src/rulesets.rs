//! Per-user push rulesets: storage and the cached read path evaluation uses.
//!
//! [`RulesetStore`] is the persistence trait (mirrors `hs_auth::store`'s shape: a trait, an
//! [`memory::InMemoryRulesetStore`] for tests, a [`tables::TablesRulesetStore`] over
//! `hs-kv`/`hs-tables` for a real `hs serve` process). A user's ruleset is stored as one JSON
//! blob (`ruma::push::Ruleset` is `Serialize`/`Deserialize` end to end) rather than exploded into
//! per-rule rows: every read and write in the spec's `/pushrules` surface (`GET` the whole thing,
//! `PUT`/`DELETE` one rule, `PUT` one rule's `actions` or `enabled`) is "read the whole ruleset,
//! mutate it in memory with `Ruleset`'s own methods (`insert`, `remove`, `set_enabled`,
//! `set_actions`), write the whole thing back" — one keyspace row, one transaction, no risk of a
//! rule and its enabled-flag landing in two separate writes that could interleave with a
//! concurrent one.
//!
//! [`CachedRulesetStore`] is what `crate::pushers` and the room-update consumer actually call: it
//! wraps a [`RulesetStore`] with a [`crate::compiled::RuleCache`], invalidating the cache after
//! every write so a user's own rule change is visible on their very next evaluation.

pub mod memory;
pub mod tables;

use std::sync::Arc;

use ruma::UserId;
use ruma::push::Ruleset;

use crate::compiled::RuleCache;
use crate::error::StoreError;

/// Persistence for per-user push rulesets.
#[async_trait::async_trait]
pub trait RulesetStore: Send + Sync {
    /// The user's stored ruleset, or `None` if they have never had one written (a brand new
    /// account, or a store that predates this user). Callers wanting "the ruleset this user
    /// should be evaluated against" should fall back to [`default_ruleset`], not treat `None` as
    /// an empty ruleset (an empty ruleset would never notify at all).
    async fn get_ruleset(&self, user_id: &UserId) -> Result<Option<Ruleset>, StoreError>;

    /// Overwrites the user's stored ruleset.
    async fn set_ruleset(&self, user_id: &UserId, ruleset: &Ruleset) -> Result<(), StoreError>;
}

/// The ruleset a user with no stored rules is evaluated against: the spec's predefined rules,
/// including the ones that depend on the user's own ID (`.m.rule.contains_user_name`,
/// `.m.rule.is_user_mention` -- see `ruma::push::Ruleset::server_default`'s own docs).
#[must_use]
pub fn default_ruleset(user_id: &UserId) -> Ruleset {
    Ruleset::server_default(user_id)
}

/// A [`RulesetStore`] fronted by a [`RuleCache`]: the seam `crate::compiled`'s module docs
/// describe. Every read goes through the cache first; every write invalidates the cache entry it
/// just changed.
pub struct CachedRulesetStore<S: RulesetStore> {
    inner: S,
    cache: Arc<RuleCache>,
}

impl<S: RulesetStore> CachedRulesetStore<S> {
    /// Wraps `inner` with a fresh cache.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            cache: Arc::new(RuleCache::new()),
        }
    }

    /// The underlying store, for callers (the `/pushrules` handlers) that need direct CRUD
    /// without the cached "effective ruleset" framing.
    pub fn store(&self) -> &S {
        &self.inner
    }

    /// The cache, shared so a caller (tests, metrics) can inspect it without going through this
    /// wrapper.
    #[must_use]
    pub fn cache(&self) -> Arc<RuleCache> {
        self.cache.clone()
    }

    /// The ruleset to evaluate `user_id` against: the cache if warm, otherwise the store's value
    /// (or [`default_ruleset`] if the store has none), caching the result either way. This is the
    /// hot-path call `crate::pushers` and the room-update consumer make once per recipient per
    /// event.
    ///
    /// # Errors
    /// Propagates the underlying store's error on a cache miss.
    pub async fn effective_ruleset(&self, user_id: &UserId) -> Result<Arc<Ruleset>, StoreError> {
        if let Some(cached) = self.cache.peek(user_id) {
            return Ok(cached);
        }
        let ruleset = match self.inner.get_ruleset(user_id).await? {
            Some(r) => r,
            None => default_ruleset(user_id),
        };
        let ruleset = Arc::new(ruleset);
        self.cache.insert(user_id.to_owned(), ruleset.clone());
        Ok(ruleset)
    }

    /// Writes `ruleset` for `user_id` and invalidates the cache so the next
    /// [`CachedRulesetStore::effective_ruleset`] call re-reads it.
    ///
    /// # Errors
    /// Propagates the underlying store's error; the cache is left untouched on failure (the old
    /// cached value, if any, is still correct since nothing was written).
    pub async fn set_ruleset(&self, user_id: &UserId, ruleset: &Ruleset) -> Result<(), StoreError> {
        self.inner.set_ruleset(user_id, ruleset).await?;
        self.cache.invalidate(user_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rulesets::memory::InMemoryRulesetStore;
    use ruma::user_id;

    #[tokio::test]
    async fn effective_ruleset_falls_back_to_default_and_caches_it() {
        let store = CachedRulesetStore::new(InMemoryRulesetStore::new());
        let alice = user_id!("@alice:example.org");
        let first = store.effective_ruleset(alice).await.unwrap();
        assert!(store.cache().peek(alice).is_some());
        let second = store.effective_ruleset(alice).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second), "second call should be served from cache");
    }

    #[tokio::test]
    async fn set_ruleset_invalidates_the_cache() {
        let store = CachedRulesetStore::new(InMemoryRulesetStore::new());
        let alice = user_id!("@alice:example.org");
        let first = store.effective_ruleset(alice).await.unwrap();

        let mut edited = (*first).clone();
        edited
            .set_enabled(ruma::push::RuleKind::Underride, ".m.rule.message", false)
            .unwrap();
        store.set_ruleset(alice, &edited).await.unwrap();
        assert!(
            store.cache().peek(alice).is_none(),
            "write must invalidate the cached entry"
        );

        let second = store.effective_ruleset(alice).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        let rule = second
            .get(ruma::push::RuleKind::Underride, ".m.rule.message")
            .unwrap();
        assert!(!rule.enabled());
    }
}
