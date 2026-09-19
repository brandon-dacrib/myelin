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

    /// Overwrites the user's stored ruleset, returning the new value of this user's change-seq
    /// (see [`RulesetStore::changed_seq`]) -- the value the write just landed at, always strictly
    /// greater than whatever [`RulesetStore::changed_seq`] returned before this call.
    async fn set_ruleset(&self, user_id: &UserId, ruleset: &Ruleset) -> Result<u64, StoreError>;

    /// This user's current push-rules change-seq: `0` if they have never had a ruleset written
    /// (the server-default ruleset, which never changes on its own), otherwise whatever the most
    /// recent [`RulesetStore::set_ruleset`] call returned. This is the seam `/sync` (track 05)
    /// needs to decide whether an incremental sync must carry `m.push_rules` again --see
    /// [`CachedRulesetStore::account_data_for_sync`] and `docs/status/10-push.md`'s "Interfaces
    /// provided".
    async fn changed_seq(&self, user_id: &UserId) -> Result<u64, StoreError>;
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
    /// [`CachedRulesetStore::effective_ruleset`] call re-reads it. Returns the new change-seq (see
    /// [`RulesetStore::changed_seq`]).
    ///
    /// # Errors
    /// Propagates the underlying store's error; the cache is left untouched on failure (the old
    /// cached value, if any, is still correct since nothing was written).
    pub async fn set_ruleset(
        &self,
        user_id: &UserId,
        ruleset: &Ruleset,
    ) -> Result<u64, StoreError> {
        let seq = self.inner.set_ruleset(user_id, ruleset).await?;
        self.cache.invalidate(user_id);
        Ok(seq)
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
    /// piece of account data.
    ///
    /// # Errors
    /// Propagates the underlying store's error.
    pub async fn account_data_for_sync(
        &self,
        user_id: &UserId,
    ) -> Result<PushRulesForSync, StoreError> {
        let ruleset = self.effective_ruleset(user_id).await?;
        let changed_seq = self.inner.changed_seq(user_id).await?;
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
                .get(ruma::push::RuleKind::Override, ".m.rule.master")
                .unwrap()
                .enabled()
        );
        for id in &override_ids[1..] {
            assert!(
                rules
                    .get(ruma::push::RuleKind::Override, *id)
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
            .set_enabled(ruma::push::RuleKind::Underride, ".m.rule.message", false)
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
