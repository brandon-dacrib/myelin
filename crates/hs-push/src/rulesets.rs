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

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, PoisonError};
use std::time::Duration;

use crate::ruleset::Ruleset;
use ruma::{OwnedUserId, UserId};

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

    /// Overwrites the user's stored ruleset, returning the new value of this user's change-seq
    /// (see [`RulesetStore::changed_seq`]) -- the value the write just landed at, always strictly
    /// greater than whatever [`RulesetStore::changed_seq`] returned before this call.
    async fn set_ruleset(&self, user_id: &UserId, ruleset: &Ruleset) -> Result<u64, StoreError>;

    /// [`RulesetStore::set_ruleset`], only if the user's change-seq is still `expected`: the
    /// write of an edit that started from the ruleset at `expected`. `Ok(None)` when another
    /// write landed since, and nothing was written. Atomic with that check, across every
    /// process sharing the store.
    async fn set_ruleset_if(
        &self,
        user_id: &UserId,
        ruleset: &Ruleset,
        expected: u64,
    ) -> Result<Option<u64>, StoreError>;

    /// This user's current push-rules change-seq: `0` if they have never had a ruleset written
    /// (the server-default ruleset, which never changes on its own), otherwise whatever the most
    /// recent [`RulesetStore::set_ruleset`] call returned. This is the seam `/sync` (track 05)
    /// needs to decide whether an incremental sync must carry `m.push_rules` again --see
    /// [`CachedRulesetStore::account_data_for_sync`] and `docs/status/10-push.md`'s "Interfaces
    /// provided".
    async fn changed_seq(&self, user_id: &UserId) -> Result<u64, StoreError>;
}

#[async_trait::async_trait]
impl<T: RulesetStore + ?Sized> RulesetStore for Arc<T> {
    async fn get_ruleset(&self, user_id: &UserId) -> Result<Option<Ruleset>, StoreError> {
        (**self).get_ruleset(user_id).await
    }

    async fn set_ruleset(&self, user_id: &UserId, ruleset: &Ruleset) -> Result<u64, StoreError> {
        (**self).set_ruleset(user_id, ruleset).await
    }

    async fn set_ruleset_if(
        &self,
        user_id: &UserId,
        ruleset: &Ruleset,
        expected: u64,
    ) -> Result<Option<u64>, StoreError> {
        (**self).set_ruleset_if(user_id, ruleset, expected).await
    }

    async fn changed_seq(&self, user_id: &UserId) -> Result<u64, StoreError> {
        (**self).changed_seq(user_id).await
    }
}

/// The ruleset a user with no stored rules is evaluated against: the spec's predefined rules,
/// including the ones that depend on the user's own ID (`.m.rule.invite_for_me`,
/// `.m.rule.is_user_mention` -- see `ruma::push::Ruleset::server_default`'s own docs).
///
/// Checked against `refs/matrix-spec/content/client-server-api/modules/push.md`'s "Predefined
/// Rules" section (v1.18, current as of this crate's Ruma pin): as of MSC4210 (spec v1.17,
/// "the legacy default push rules that looked for mentions in the body of the event were
/// removed"), the normative default list has **no** `.m.rule.contains_user_name` (a content
/// rule), `.m.rule.contains_display_name` or `.m.rule.roomnotif` (override rules) -- superseded
/// by `.m.rule.is_user_mention`/`.m.rule.is_room_mention`. `ruma-common` 0.18's
/// `Ruleset::server_default` (the version this crate's Cargo.lock actually resolves to --
/// `cargo tree -p hs-push -i ruma-common`) already matches this exactly: same ten override rules
/// in the same order, no content rules, the same five underride rules in the same order, every
/// action's wire encoding (bare `{"set_tweak":"highlight"}` for the implied-`true` case, etc.)
/// verified byte-for-byte against the spec's own JSON examples in that section. See
/// `rulesets::tests::default_ruleset_matches_the_spec_predefined_rule_list` for the regression
/// test and `docs/status/10-push.md`'s "Decisions made" for the comparison against
/// `refs/synapse/rust/src/push/base_rules.rs`, which still ships those three retired rules plus
/// unstable extras (MSC3664's `.im.nheko.msc3664.reply`, MSC4028's
/// `.org.matrix.msc4028.encrypted_event`) this crate does not yet add -- tracked, not silently
/// dropped.
#[must_use]
pub fn default_ruleset(user_id: &UserId) -> Ruleset {
    Ruleset::server_default(user_id)
}

/// How many times [`CachedRulesetStore::update_ruleset`] starts a change over because the
/// ruleset was changed elsewhere (another replica) while it was being edited.
const UPDATE_ATTEMPTS: usize = 8;

/// Tells the other processes sharing the store that a user's push rules changed, so they drop
/// their cached copy (`crate::compiled`'s "Across replicas"). `hs-cli` sends it over the
/// cluster mesh; a single node installs none.
pub trait RulesetChangeFeed: Send + Sync {
    /// `user_id`'s rules were written here, at change-seq `seq`. Must not block: delivery is
    /// best effort and happens in the background.
    fn changed(&self, user_id: &UserId, seq: u64);
}

/// One write lock per user with a write under way, so a user's rule writes are made one at a
/// time without any user waiting on another's. A user's lock exists only while a write holds
/// or waits for it: the last guard out removes it.
#[derive(Default)]
pub struct UserWriteLocks {
    locks: std::sync::Mutex<HashMap<OwnedUserId, Arc<tokio::sync::Mutex<()>>>>,
}

impl UserWriteLocks {
    /// Waits for `user_id`'s turn to write.
    pub async fn lock(&self, user_id: &UserId) -> UserWriteGuard<'_> {
        let lock = self
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(user_id.to_owned())
            .or_default()
            .clone();
        // `lock` goes into the guard: once the guard is dropped, only the map and any waiter
        // hold the user's lock.
        let guard = lock.lock_owned().await;
        UserWriteGuard {
            guard: Some(guard),
            locks: self,
            user_id: user_id.to_owned(),
        }
    }

    /// Drops `user_id`'s lock once nothing holds or waits for it.
    fn release(&self, user_id: &UserId) {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        if locks
            .get(user_id)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            locks.remove(user_id);
        }
    }

    /// How many users have a write under way or waiting (tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// No user has a write under way.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for UserWriteLocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserWriteLocks")
            .field("users", &self.len())
            .finish()
    }
}

/// One user's turn to write, from [`UserWriteLocks::lock`]; the turn ends when it is dropped.
pub struct UserWriteGuard<'a> {
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    locks: &'a UserWriteLocks,
    user_id: OwnedUserId,
}

impl Drop for UserWriteGuard<'_> {
    fn drop(&mut self) {
        // The mutex first (its `Arc` goes with the guard), then the map entry if nobody waits.
        self.guard.take();
        self.locks.release(&self.user_id);
    }
}

/// A [`RulesetStore`] fronted by a [`RuleCache`]: the seam `crate::compiled`'s module docs
/// describe. Every read goes through the cache first; every write invalidates the cache entry it
/// just changed, and tells the other replicas when there are any ([`RulesetChangeFeed`]).
///
/// Every change to a ruleset is a read, an edit and a write of the whole thing, so two changes
/// made at once (a client adding rules for two rooms in parallel, or a rule copied onto an
/// upgraded room while the user edits another) must not both start from the same read: the
/// second write would drop the first one's rule. [`CachedRulesetStore::update_ruleset`] makes
/// each change under one lock, reading the store rather than the cache, and
/// [`CachedRulesetStore::set_ruleset`] takes the same lock. The lock is per user
/// ([`UserWriteLocks`]): one user's slow write never holds up another's, which matters when
/// every member of a big room joins its upgraded replacement at once and each join copies
/// that member's rules. The lock covers one process; between replicas the write itself is
/// conditional on the change-seq the edit started from ([`RulesetStore::set_ruleset_if`]),
/// and an edit that lost starts over.
pub struct CachedRulesetStore<S: RulesetStore> {
    inner: S,
    cache: Arc<RuleCache>,
    writes: UserWriteLocks,
    /// Told of every write, in a cluster.
    feed: OnceLock<Arc<dyn RulesetChangeFeed>>,
    /// How long an entry is used on the evaluation path before its seq is checked against
    /// the store again; unset (never) on a single node.
    revalidate_after: OnceLock<Duration>,
}

impl<S: RulesetStore> CachedRulesetStore<S> {
    /// Wraps `inner` with a fresh, empty cache.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            cache: Arc::new(RuleCache::new()),
            writes: UserWriteLocks::default(),
            feed: OnceLock::new(),
            revalidate_after: OnceLock::new(),
        }
    }

    /// Makes this store cluster-aware: every write is told to `feed`, and a cached entry used
    /// for evaluation is checked against the store once it is `revalidate_after` old (the
    /// bound on staleness should a change's message be lost). Set once; a second call is
    /// ignored and logged.
    pub fn install_change_feed(
        &self,
        feed: Arc<dyn RulesetChangeFeed>,
        revalidate_after: Duration,
    ) {
        if self.feed.set(feed).is_err() || self.revalidate_after.set(revalidate_after).is_err() {
            tracing::warn!("a push-rule change feed was already installed; keeping the first");
        }
    }

    /// Another replica wrote `user_id`'s rules (at `seq`): drops the cached copy, unless it is
    /// already that one or newer.
    pub fn changed_elsewhere(&self, user_id: &UserId, seq: u64) {
        if self
            .cache
            .entry(user_id)
            .is_some_and(|e| e.seq.is_some_and(|held| held >= seq))
        {
            return;
        }
        self.cache.invalidate(user_id);
        crate::compiled::count_invalidation("peer");
        tracing::debug!(user = %user_id, seq, "push rules changed on another replica; cached copy dropped");
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
    /// event. In a cluster, an entry older than the revalidation interval has its seq checked
    /// against the store first.
    ///
    /// # Errors
    /// Propagates the underlying store's error on a cache miss or a check.
    pub async fn effective_ruleset(&self, user_id: &UserId) -> Result<Arc<Ruleset>, StoreError> {
        if let Some(entry) = self.cache.entry(user_id) {
            let Some(after) = self.revalidate_after.get() else {
                return Ok(entry.ruleset);
            };
            if entry.checked.elapsed() < *after {
                return Ok(entry.ruleset);
            }
            let seq = self.inner.changed_seq(user_id).await?;
            if entry.seq == Some(seq) {
                self.cache.confirm(user_id, seq);
                return Ok(entry.ruleset);
            }
            crate::compiled::count_invalidation("stale");
            tracing::debug!(user = %user_id, seq, "cached push rules were behind the store; read again");
        }
        Ok(self.load(user_id).await?.0)
    }

    /// The ruleset as the store has it now, from the cache when the cached copy is current
    /// (one change-seq read to know), with that change-seq. What a client is shown (`/sync`'s
    /// `m.push_rules`, `GET /pushrules`), so never a stale copy, cluster or not.
    ///
    /// # Errors
    /// Propagates the underlying store's error.
    pub async fn current_ruleset(
        &self,
        user_id: &UserId,
    ) -> Result<(Arc<Ruleset>, u64), StoreError> {
        let seq = self.inner.changed_seq(user_id).await?;
        if let Some(entry) = self.cache.entry(user_id)
            && entry.seq == Some(seq)
        {
            self.cache.confirm(user_id, seq);
            return Ok((entry.ruleset, seq));
        }
        if self.cache.entry(user_id).is_some() {
            crate::compiled::count_invalidation("stale");
            tracing::debug!(user = %user_id, seq, "cached push rules were behind the store; read again");
        }
        self.load(user_id).await
    }

    /// Reads `user_id`'s ruleset and change-seq from the store and caches them, unless a write
    /// landed meanwhile. The seq is read first: a write between the two reads leaves an entry
    /// whose seq is behind its ruleset, which the next check reads again (never the reverse).
    async fn load(&self, user_id: &UserId) -> Result<(Arc<Ruleset>, u64), StoreError> {
        // Taken before the read: a write that lands between the read and the insert below
        // keeps what was read out of the cache (`RuleCache::insert_if_unchanged`).
        let generation = self.cache.generation();
        let seq = self.inner.changed_seq(user_id).await?;
        let ruleset = match self.inner.get_ruleset(user_id).await? {
            Some(r) => r,
            None => default_ruleset(user_id),
        };
        let ruleset = Arc::new(ruleset);
        if !self.cache.insert_if_unchanged(
            user_id.to_owned(),
            ruleset.clone(),
            Some(seq),
            generation,
        ) {
            tracing::debug!(user = %user_id, "push rules changed while being read; not cached");
        }
        Ok((ruleset, seq))
    }

    /// Writes `user_id`'s ruleset through to the store and invalidates the cache entry, so the
    /// next [`CachedRulesetStore::effective_ruleset`] call sees the new rules. Returns the new
    /// change-seq.
    ///
    /// # Errors
    /// Propagates the underlying store's error.
    pub async fn set_ruleset(
        &self,
        user_id: &UserId,
        ruleset: &Ruleset,
    ) -> Result<u64, StoreError> {
        let _write = self.writes.lock(user_id).await;
        let seq = self.inner.set_ruleset(user_id, ruleset).await?;
        self.written(user_id, seq);
        Ok(seq)
    }

    /// What every write is followed by: the cached copy goes, and the other replicas are told.
    fn written(&self, user_id: &UserId, seq: u64) {
        self.cache.invalidate(user_id);
        crate::compiled::count_invalidation("local");
        if let Some(feed) = self.feed.get() {
            feed.changed(user_id, seq);
        }
    }

    /// Changes `user_id`'s ruleset with `edit`, one change at a time: `edit` is given the
    /// stored ruleset (or [`default_ruleset`]) as of now, read under the lock every write here
    /// takes, so a change made at the same time cannot be lost under this one; and the write
    /// lands only if nothing (another replica) wrote since that read, else `edit` is given the
    /// newer ruleset and runs again. `Ok(Some(value))` from `edit` writes the edited ruleset
    /// and returns `value` with the new change-seq; `Ok(None)` writes nothing; an error writes
    /// nothing and is returned.
    ///
    /// # Errors
    /// `edit`'s error, or the underlying store's (converted with `From`), or a store error
    /// when the ruleset kept changing elsewhere through every attempt.
    pub async fn update_ruleset<T, E>(
        &self,
        user_id: &UserId,
        mut edit: impl FnMut(&mut Ruleset) -> Result<Option<T>, E>,
    ) -> Result<Option<(T, u64)>, E>
    where
        E: From<StoreError>,
    {
        let _write = self.writes.lock(user_id).await;
        for attempt in 1..=UPDATE_ATTEMPTS {
            // The seq first: a write between the two reads makes the conditional write below
            // fail, and the edit runs again on what that write left.
            let expected = self.inner.changed_seq(user_id).await?;
            let mut ruleset = match self.inner.get_ruleset(user_id).await? {
                Some(r) => r,
                None => default_ruleset(user_id),
            };
            let Some(value) = edit(&mut ruleset)? else {
                return Ok(None);
            };
            if let Some(seq) = self
                .inner
                .set_ruleset_if(user_id, &ruleset, expected)
                .await?
            {
                self.written(user_id, seq);
                return Ok(Some((value, seq)));
            }
            tracing::debug!(
                user = %user_id,
                attempt,
                "push rules changed elsewhere while being edited; editing them again"
            );
        }
        Err(StoreError::Backend(format!(
            "{user_id}'s push rules kept changing elsewhere through {UPDATE_ATTEMPTS} attempts"
        ))
        .into())
    }

    /// Everything `/sync` (track 05) needs to decide whether, and what, to include for
    /// `m.push_rules`: the account-data event's `content` in exactly the wire shape the spec
    /// requires, plus the change-seq it reflects.
    ///
    /// This always returns a value -- a user with no stored ruleset still has an effective one
    /// (the server default) and a well-defined change-seq (`0`, meaning "never changed"). Track
    /// 05 decides whether to actually emit it in a given response: unconditionally on an initial
    /// sync, or when `changed_seq` is strictly greater than the baseline carried in the sync
    /// token on an incremental one -- exactly the comparison `crate::rulesets`'s module docs and
    /// `docs/status/10-push.md` describe, and the same shape `crate::store::UserStore`'s own
    /// `account_data_seq`/`changed_seq` convention in `hs-user` already uses for every other
    /// piece of account data. The content is the store's as of `changed_seq`
    /// ([`CachedRulesetStore::current_ruleset`]), whichever replica wrote it.
    ///
    /// # Errors
    /// Propagates the underlying store's error.
    pub async fn account_data_for_sync(
        &self,
        user_id: &UserId,
    ) -> Result<PushRulesForSync, StoreError> {
        let (ruleset, changed_seq) = self.current_ruleset(user_id).await?;
        Ok(PushRulesForSync {
            content: account_data_content(&ruleset),
            changed_seq,
        })
    }
}

/// The `m.push_rules` account-data payload for one user (see
/// [`CachedRulesetStore::account_data_for_sync`]).
#[derive(Debug, Clone)]
pub struct PushRulesForSync {
    /// The event's `content` field: `{"global": {...}}`, built by [`account_data_content`].
    pub content: serde_json::Value,
    /// The change-seq this content reflects, comparable against a baseline carried in a sync
    /// token the same way `hs_user`'s own account-data seq is (see
    /// [`CachedRulesetStore::account_data_for_sync`]'s doc comment).
    pub changed_seq: u64,
}

/// Builds the `m.push_rules` event's `content` field from `ruleset`: `{"global": ruleset}`. This
/// is exactly the shape `GET /pushrules/` already returns (`crate::routes::pushrules`) and the
/// one the spec's `m.push_rules` account data event requires
/// (`data/event-schemas/schema/m.push_rules.yaml`, and see the client-server spec's "Push Rules:
/// Events" section) -- `ruma::push::Ruleset`'s own `Serialize` impl already produces the wire
/// field names (`content`, `override`, `room`, `sender`, `underride`) verbatim, so no further
/// reshaping is needed here.
#[must_use]
pub fn account_data_content(ruleset: &Ruleset) -> serde_json::Value {
    serde_json::json!({ "global": ruleset })
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
        assert!(
            Arc::ptr_eq(&first, &second),
            "second call should be served from cache"
        );
    }

    #[tokio::test]
    async fn set_ruleset_invalidates_the_cache() {
        let store = CachedRulesetStore::new(InMemoryRulesetStore::new());
        let alice = user_id!("@alice:example.org");
        let first = store.effective_ruleset(alice).await.unwrap();

        let mut edited = (*first).clone();
        edited
            .set_enabled(
                crate::ruleset::RuleKind::Underride,
                ".m.rule.message",
                false,
            )
            .unwrap();
        store.set_ruleset(alice, &edited).await.unwrap();
        assert!(
            store.cache().peek(alice).is_none(),
            "write must invalidate the cached entry"
        );

        let second = store.effective_ruleset(alice).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        let rule = second
            .get(crate::ruleset::RuleKind::Underride, ".m.rule.message")
            .unwrap();
        assert!(!rule.enabled());
    }

    /// A store whose reads take a while, so two changes made at once both read before either
    /// writes unless something keeps them apart.
    struct SlowReads(InMemoryRulesetStore);

    #[async_trait::async_trait]
    impl RulesetStore for SlowReads {
        async fn get_ruleset(&self, user_id: &UserId) -> Result<Option<Ruleset>, StoreError> {
            let read = self.0.get_ruleset(user_id).await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            read
        }
        async fn set_ruleset(
            &self,
            user_id: &UserId,
            ruleset: &Ruleset,
        ) -> Result<u64, StoreError> {
            self.0.set_ruleset(user_id, ruleset).await
        }
        async fn set_ruleset_if(
            &self,
            user_id: &UserId,
            ruleset: &Ruleset,
            expected: u64,
        ) -> Result<Option<u64>, StoreError> {
            self.0.set_ruleset_if(user_id, ruleset, expected).await
        }
        async fn changed_seq(&self, user_id: &UserId) -> Result<u64, StoreError> {
            self.0.changed_seq(user_id).await
        }
    }

    /// A store whose reads of one user wait until told to go on: a slow write for that user.
    struct HeldUser {
        inner: InMemoryRulesetStore,
        held: ruma::OwnedUserId,
        go: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl RulesetStore for HeldUser {
        async fn get_ruleset(&self, user_id: &UserId) -> Result<Option<Ruleset>, StoreError> {
            if user_id == self.held {
                self.go.notified().await;
            }
            self.inner.get_ruleset(user_id).await
        }
        async fn set_ruleset(
            &self,
            user_id: &UserId,
            ruleset: &Ruleset,
        ) -> Result<u64, StoreError> {
            self.inner.set_ruleset(user_id, ruleset).await
        }
        async fn set_ruleset_if(
            &self,
            user_id: &UserId,
            ruleset: &Ruleset,
            expected: u64,
        ) -> Result<Option<u64>, StoreError> {
            self.inner.set_ruleset_if(user_id, ruleset, expected).await
        }
        async fn changed_seq(&self, user_id: &UserId) -> Result<u64, StoreError> {
            self.inner.changed_seq(user_id).await
        }
    }

    /// The write lock is per user: bob's edit lands while alice's is stuck in the store, and a
    /// user's lock exists only while a write of theirs is under way.
    #[tokio::test]
    async fn one_users_slow_write_does_not_hold_up_anothers() {
        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        let go = Arc::new(tokio::sync::Notify::new());
        let store = Arc::new(CachedRulesetStore::new(HeldUser {
            inner: InMemoryRulesetStore::new(),
            held: alice.to_owned(),
            go: go.clone(),
        }));
        let alices = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .update_ruleset::<_, StoreError>(alice, |ruleset| {
                        ruleset
                            .insert(room_rule("!a:example.org"), None, None)
                            .unwrap();
                        Ok(Some(()))
                    })
                    .await
                    .unwrap()
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(store.writes.len(), 1, "alice's write holds her lock");

        let bobs = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            store.update_ruleset::<_, StoreError>(bob, |ruleset| {
                ruleset
                    .insert(room_rule("!b:example.org"), None, None)
                    .unwrap();
                Ok(Some(()))
            }),
        )
        .await
        .expect("bob's write must not wait for alice's")
        .unwrap();
        assert!(bobs.is_some());
        assert_eq!(store.writes.len(), 1, "bob's lock went with his write");

        go.notify_one();
        assert!(alices.await.unwrap().is_some());
        assert!(store.writes.is_empty(), "alice's lock went with hers");
        // Read beneath `HeldUser`, which would wait for alice again.
        let plain = &store.inner.inner;
        assert_eq!(
            plain.get_ruleset(alice).await.unwrap().unwrap().room.len(),
            1
        );
        assert_eq!(plain.get_ruleset(bob).await.unwrap().unwrap().room.len(), 1);
    }

    /// A store that another replica writes to, once, right after this one's first read of the
    /// ruleset: what a change made on two replicas at once looks like from one of them.
    struct WrittenElsewhere {
        inner: InMemoryRulesetStore,
        pending: std::sync::Mutex<Option<Ruleset>>,
    }

    #[async_trait::async_trait]
    impl RulesetStore for WrittenElsewhere {
        async fn get_ruleset(&self, user_id: &UserId) -> Result<Option<Ruleset>, StoreError> {
            let read = self.inner.get_ruleset(user_id).await;
            let elsewhere = self.pending.lock().unwrap().take();
            if let Some(ruleset) = elsewhere {
                self.inner.set_ruleset(user_id, &ruleset).await?;
            }
            read
        }
        async fn set_ruleset(
            &self,
            user_id: &UserId,
            ruleset: &Ruleset,
        ) -> Result<u64, StoreError> {
            self.inner.set_ruleset(user_id, ruleset).await
        }
        async fn set_ruleset_if(
            &self,
            user_id: &UserId,
            ruleset: &Ruleset,
            expected: u64,
        ) -> Result<Option<u64>, StoreError> {
            self.inner.set_ruleset_if(user_id, ruleset, expected).await
        }
        async fn changed_seq(&self, user_id: &UserId) -> Result<u64, StoreError> {
            self.inner.changed_seq(user_id).await
        }
    }

    /// A change another replica made while this one was editing is kept: this replica's
    /// write is refused, and its edit runs again on the ruleset that change left.
    #[tokio::test]
    async fn a_change_made_on_another_replica_meanwhile_is_kept() {
        let alice = user_id!("@alice:example.org");
        let mut theirs = default_ruleset(alice);
        theirs
            .insert(room_rule("!theirs:example.org"), None, None)
            .unwrap();
        let store = CachedRulesetStore::new(WrittenElsewhere {
            inner: InMemoryRulesetStore::new(),
            pending: std::sync::Mutex::new(Some(theirs)),
        });
        let mut runs = 0;
        let (_, seq) = store
            .update_ruleset(alice, |ruleset| {
                runs += 1;
                ruleset
                    .insert(room_rule("!mine:example.org"), None, None)
                    .map(Some)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(runs, 2, "the edit ran again on the newer ruleset");
        assert_eq!(seq, 2);
        let (ruleset, _) = store.current_ruleset(alice).await.unwrap();
        for room in ["!theirs:example.org", "!mine:example.org"] {
            assert!(
                ruleset.get(crate::ruleset::RuleKind::Room, room).is_some(),
                "{room}'s rule was lost"
            );
        }
    }

    /// Records what a [`CachedRulesetStore`] tells the other replicas.
    #[derive(Default)]
    struct RecordingFeed(std::sync::Mutex<Vec<(ruma::OwnedUserId, u64)>>);

    impl RulesetChangeFeed for RecordingFeed {
        fn changed(&self, user_id: &UserId, seq: u64) {
            self.0.lock().unwrap().push((user_id.to_owned(), seq));
        }
    }

    /// Two replicas over one store: a change on one is told to the other, which drops its
    /// copy; what clients are shown is never stale even without that message; and the
    /// evaluation path reads the store again once its copy is past the revalidation interval.
    #[tokio::test]
    async fn a_change_on_one_replica_reaches_the_others_cache() {
        let alice = user_id!("@alice:example.org");
        let shared = Arc::new(InMemoryRulesetStore::new());
        let a = CachedRulesetStore::new(shared.clone());
        let b = CachedRulesetStore::new(shared.clone());
        let feed = Arc::new(RecordingFeed::default());
        a.install_change_feed(feed.clone(), Duration::from_secs(3600));
        b.install_change_feed(
            Arc::new(RecordingFeed::default()),
            Duration::from_secs(3600),
        );

        // B has alice's rules cached.
        assert!(b.effective_ruleset(alice).await.unwrap().room.is_empty());
        let add = |ruleset: &mut Ruleset| {
            ruleset
                .insert(room_rule("!one:example.org"), None, None)
                .map(Some)
                .map_err(|e| StoreError::Backend(e.to_string()))
        };
        let (_, seq) = a.update_ruleset(alice, add).await.unwrap().unwrap();
        assert_eq!(*feed.0.lock().unwrap(), [(alice.to_owned(), seq)]);

        // Before the message reaches B: B's evaluation copy is stale (an hour's interval),
        // but what a client is shown is read through to the store.
        assert!(b.effective_ruleset(alice).await.unwrap().room.is_empty());
        let (shown, shown_seq) = b.current_ruleset(alice).await.unwrap();
        assert_eq!(shown.room.len(), 1);
        assert_eq!(shown_seq, seq);
        let sync = b.account_data_for_sync(alice).await.unwrap();
        assert_eq!(sync.changed_seq, seq);

        // The message: B's next evaluation reads the new rules.
        b.cache().invalidate(alice);
        b.effective_ruleset(alice).await.unwrap();
        a.update_ruleset(alice, |ruleset| {
            ruleset
                .remove(crate::ruleset::RuleKind::Room, "!one:example.org")
                .map(|()| Some(()))
                .map_err(|e| StoreError::Backend(e.to_string()))
        })
        .await
        .unwrap();
        let (told, seq) = feed.0.lock().unwrap().last().cloned().unwrap();
        b.changed_elsewhere(&told, seq);
        assert!(b.cache().peek(alice).is_none());
        assert!(b.effective_ruleset(alice).await.unwrap().room.is_empty());
        // A message about a change B already has is ignored.
        b.changed_elsewhere(alice, seq);
        assert!(b.cache().peek(alice).is_some());
    }

    /// With no message at all, the evaluation path notices a change made elsewhere once its
    /// copy is past the revalidation interval.
    #[tokio::test]
    async fn a_lost_message_is_bounded_by_the_revalidation_interval() {
        let alice = user_id!("@alice:example.org");
        let shared = Arc::new(InMemoryRulesetStore::new());
        let b = CachedRulesetStore::new(shared.clone());
        b.install_change_feed(Arc::new(RecordingFeed::default()), Duration::ZERO);
        assert!(b.effective_ruleset(alice).await.unwrap().room.is_empty());
        let mut edited = default_ruleset(alice);
        edited
            .insert(room_rule("!one:example.org"), None, None)
            .unwrap();
        shared.set_ruleset(alice, &edited).await.unwrap();
        assert_eq!(b.effective_ruleset(alice).await.unwrap().room.len(), 1);
        // And an unchanged copy is kept, its check renewed.
        let before = b.cache().entry(alice).unwrap().checked;
        b.effective_ruleset(alice).await.unwrap();
        assert!(b.cache().entry(alice).unwrap().checked >= before);
        assert_eq!(b.cache().entry(alice).unwrap().seq, Some(1));
    }

    fn room_rule(room: &str) -> crate::ruleset::NewRule {
        crate::ruleset::NewRule {
            kind: crate::ruleset::RuleKind::Room,
            rule_id: room.to_owned(),
            actions: Vec::new(),
            conditions: Vec::new(),
            pattern: None,
        }
    }

    /// Complement's `TestPushRuleRoomUpgrade` adds one user's room rules for two rooms from two
    /// parallel subtests; each change read the ruleset, added its rule and wrote it back, and
    /// the later write dropped the earlier rule, which that user's `/sync` then waited for in
    /// vain.
    #[tokio::test]
    async fn two_changes_made_at_once_both_land() {
        let store = Arc::new(CachedRulesetStore::new(SlowReads(
            InMemoryRulesetStore::new(),
        )));
        let alice = user_id!("@alice:example.org");
        let add = |room: &'static str| {
            let store = store.clone();
            async move {
                store
                    .update_ruleset(alice, |ruleset| {
                        ruleset
                            .insert(room_rule(room), None, None)
                            .map(Some)
                            .map_err(|e| StoreError::Backend(e.to_string()))
                    })
                    .await
            }
        };
        let (first, second) = tokio::join!(add("!one:example.org"), add("!two:example.org"));
        first.unwrap().expect("the first change was written");
        second.unwrap().expect("the second change was written");
        let ruleset = store.effective_ruleset(alice).await.unwrap();
        for room in ["!one:example.org", "!two:example.org"] {
            assert!(
                ruleset.get(crate::ruleset::RuleKind::Room, room).is_some(),
                "the rule for {room} was lost to the other change"
            );
        }
        assert_eq!(store.store().changed_seq(alice).await.unwrap(), 2);
    }

    /// A reader that misses the cache while a change is being written must not cache what it
    /// read before the write: every later read would be served the ruleset without the change.
    #[tokio::test]
    async fn a_read_racing_a_change_does_not_cache_the_old_rules() {
        let store = Arc::new(CachedRulesetStore::new(SlowReads(
            InMemoryRulesetStore::new(),
        )));
        let alice = user_id!("@alice:example.org");
        let reader = {
            let store = store.clone();
            tokio::spawn(async move { store.effective_ruleset(alice).await })
        };
        // The reader is inside its slow read; the change lands while it waits.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let mut edited = default_ruleset(alice);
        edited
            .insert(room_rule("!one:example.org"), None, None)
            .unwrap();
        store.store().set_ruleset(alice, &edited).await.unwrap();
        store.cache().invalidate(alice);
        reader.await.unwrap().unwrap();
        let after = store.effective_ruleset(alice).await.unwrap();
        assert!(
            after
                .get(crate::ruleset::RuleKind::Room, "!one:example.org")
                .is_some(),
            "the read from before the change was cached over it"
        );
    }

    #[tokio::test]
    async fn an_edit_that_changes_nothing_writes_nothing() {
        let store = CachedRulesetStore::new(InMemoryRulesetStore::new());
        let alice = user_id!("@alice:example.org");
        let outcome = store
            .update_ruleset::<(), StoreError>(alice, |_| Ok(None))
            .await
            .unwrap();
        assert!(outcome.is_none());
        assert_eq!(store.store().changed_seq(alice).await.unwrap(), 0);
    }

    /// Pins [`default_ruleset`]'s shape against the spec's own "Predefined Rules" list
    /// (`refs/matrix-spec/content/client-server-api/modules/push.md`, "Default Override Rules"
    /// and "Default Underride Rules" sections, v1.18) so a future Ruma upgrade that silently
    /// changes, reorders or reintroduces a retired rule (MSC4210) fails a test here rather than
    /// being noticed only when a client's notification behavior looks wrong. Priority order
    /// matters (`ruma::push::Ruleset::iter` evaluates override rules in list order, first match
    /// wins for `.m.rule.master`'s "always highest" guarantee to hold), so this checks order, not
    /// just membership.
    #[test]
    fn default_ruleset_matches_the_spec_predefined_rule_list() {
        let alice = user_id!("@alice:example.org");
        let rules = default_ruleset(alice);

        let override_ids: Vec<&str> = rules.override_.iter().map(|r| r.rule_id.as_str()).collect();
        assert_eq!(
            override_ids,
            vec![
                ".m.rule.master",
                ".m.rule.suppress_notices",
                ".m.rule.invite_for_me",
                ".m.rule.member_event",
                ".m.rule.is_user_mention",
                ".m.rule.is_room_mention",
                ".m.rule.tombstone",
                ".m.rule.reaction",
                ".m.rule.room.server_acl",
                ".m.rule.suppress_edits",
            ],
            "override rules must match the spec's list, in the spec's priority order, with no \
             MSC4210-retired rule (contains_display_name, roomnotif) reintroduced"
        );

        assert!(
            rules.content.is_empty(),
            "the spec defines no default content rules since MSC4210 retired \
             .m.rule.contains_user_name"
        );
        assert!(
            rules.room.is_empty() && rules.sender.is_empty(),
            "room/sender rules are always user-added only, never server-default"
        );

        let underride_ids: Vec<&str> = rules.underride.iter().map(|r| r.rule_id.as_str()).collect();
        assert_eq!(
            underride_ids,
            vec![
                ".m.rule.call",
                ".m.rule.encrypted_room_one_to_one",
                ".m.rule.room_one_to_one",
                ".m.rule.message",
                ".m.rule.encrypted",
            ],
            "underride rules must match the spec's list, in the spec's priority order"
        );

        // `.m.rule.master` must default to disabled (an operator/client enabling it silences
        // everything); every other rule defaults to enabled.
        assert!(
            !rules
                .get(crate::ruleset::RuleKind::Override, ".m.rule.master")
                .unwrap()
                .enabled()
        );
        for id in &override_ids[1..] {
            assert!(
                rules
                    .get(crate::ruleset::RuleKind::Override, id)
                    .unwrap()
                    .enabled(),
                "{id} must default to enabled"
            );
        }
    }

    /// The seam `docs/status/10-push.md` documents for track 05: a user who has never customized
    /// their rules has a well-defined change-seq of `0` (never changed), a customization strictly
    /// advances it, and `account_data_for_sync` reports both in the wire shape `/sync` needs.
    #[tokio::test]
    async fn account_data_for_sync_tracks_the_change_seq() {
        let store = CachedRulesetStore::new(InMemoryRulesetStore::new());
        let alice = user_id!("@alice:example.org");

        let before = store.account_data_for_sync(alice).await.unwrap();
        assert_eq!(before.changed_seq, 0, "never customized: seq stays at 0");
        assert_eq!(
            before.content["global"]["underride"]
                .as_array()
                .unwrap()
                .len(),
            5,
            "the default ruleset's underride rules should be present verbatim"
        );

        let mut edited = default_ruleset(alice);
        edited
            .set_enabled(
                crate::ruleset::RuleKind::Underride,
                ".m.rule.message",
                false,
            )
            .unwrap();
        let written_seq = store.set_ruleset(alice, &edited).await.unwrap();
        assert!(written_seq > 0);

        let after = store.account_data_for_sync(alice).await.unwrap();
        assert_eq!(after.changed_seq, written_seq);
        assert!(after.changed_seq > before.changed_seq);
        assert_eq!(
            after.content["global"]["underride"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["rule_id"] == ".m.rule.message")
                .unwrap()["enabled"],
            false
        );

        // A second write strictly advances the seq again -- not just "greater than the initial
        // 0", the actual monotonic property `/sync`'s baseline comparison depends on.
        let seq_after_second_write = store.set_ruleset(alice, &edited).await.unwrap();
        assert!(seq_after_second_write > written_seq);
    }

    /// `account_data_content`'s wire shape must be exactly `{"global": <Ruleset>}`: the same
    /// `global` field, of the same `ruma::push::Ruleset` type, that
    /// `ruma::api::client::push::get_pushrules_all::v3::Response` -- the type a real client
    /// deserializes `GET /pushrules/`'s body with -- carries (`Response { pub global: Ruleset }`,
    /// see that module's source). This fails if `account_data_content` ever nests, renames or
    /// reshapes the field, since a real client would then be unable to read either endpoint.
    #[test]
    fn account_data_content_has_exactly_the_client_facing_global_field() {
        let alice = user_id!("@alice:example.org");
        let content = account_data_content(&default_ruleset(alice));
        let object = content.as_object().expect("content must be a JSON object");
        assert_eq!(
            object.keys().collect::<Vec<_>>(),
            vec!["global"],
            "content must have exactly one field, `global` (matching \
             ruma::api::client::push::get_pushrules_all::v3::Response)"
        );
        let global: Ruleset = serde_json::from_value(object["global"].clone())
            .expect("content.global must parse as ruma::push::Ruleset");
        assert!(
            global
                .underride
                .iter()
                .any(|r| r.rule_id.as_str() == ".m.rule.message")
        );
    }
}
