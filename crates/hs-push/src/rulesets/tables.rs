//! An `hs-kv`/`hs-tables`-backed [`super::RulesetStore`], following the pattern
//! `crates/hs-auth/src/store/tables.rs` set: one keyspace, typed primary key, JSON-encoded row.

use crate::ruleset::Ruleset;
use hs_kv::{KvBackend, KvRead, KvWrite, TransactConfig, transact};
use hs_tables::keyspace::TypedKeyspace;
use ruma::UserId;

use super::RulesetStore;
use crate::error::StoreError;

/// | keyspace | primary key | value |
/// |---|---|---|
/// | `hs_push.rulesets` | `(user_id,)` | the user's `Ruleset`, JSON-encoded |
/// | `hs_push.ruleset_seq` | `user_id` (raw `atomic_add` key, not a [`TypedKeyspace`]) | the
/// user's push-rules change-seq -- mirrors `hs_user.account_data_counter`'s shape
/// (`crates/hs-user/src/store/tables.rs`) for the identical reason: `/sync` needs a cheap
/// "did this change since token T" comparison without decoding the ruleset itself. |
pub struct TablesRulesetStore<B: KvBackend> {
    backend: B,
    rulesets: TypedKeyspace<B::Keyspace, (String,)>,
    ruleset_seq: B::Keyspace,
}

impl<B: KvBackend> TablesRulesetStore<B> {
    /// Opens (creating if necessary) this store's keyspaces.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if a keyspace could not be opened.
    pub fn open(backend: B) -> Result<Self, StoreError> {
        let rulesets = TypedKeyspace::new(
            backend
                .keyspace("hs_push.rulesets")
                .map_err(|e| StoreError::Backend(e.to_string()))?,
        );
        let ruleset_seq = backend
            .keyspace("hs_push.ruleset_seq")
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(Self {
            backend,
            rulesets,
            ruleset_seq,
        })
    }
}

#[async_trait::async_trait]
impl<B: KvBackend> RulesetStore for TablesRulesetStore<B> {
    async fn get_ruleset(&self, user_id: &UserId) -> Result<Option<Ruleset>, StoreError> {
        let snap = self.backend.snapshot();
        let Some(bytes) = self.rulesets.get(&snap, &(user_id.to_string(),))? else {
            return Ok(None);
        };
        let ruleset: Ruleset = serde_json::from_slice(&bytes)
            .map_err(|e| StoreError::Backend(format!("decode ruleset: {e}")))?;
        Ok(Some(ruleset))
    }

    async fn set_ruleset(&self, user_id: &UserId, ruleset: &Ruleset) -> Result<u64, StoreError> {
        let key = (user_id.to_string(),);
        let uid_bytes = user_id.as_bytes().to_vec();
        let value = serde_json::to_vec(ruleset)
            .map_err(|e| StoreError::Backend(format!("encode ruleset: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.rulesets
                .put(txn, &key, &value)
                .map_err(hs_kv::KvError::backend)?;
            let seq = txn.atomic_add(&self.ruleset_seq, &uid_bytes, 1)?;
            #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
            Ok(seq as u64)
        })
        .map_err(StoreError::from)
    }

    async fn set_ruleset_if(
        &self,
        user_id: &UserId,
        ruleset: &Ruleset,
        expected: u64,
    ) -> Result<Option<u64>, StoreError> {
        let key = (user_id.to_string(),);
        let uid_bytes = user_id.as_bytes().to_vec();
        let value = serde_json::to_vec(ruleset)
            .map_err(|e| StoreError::Backend(format!("encode ruleset: {e}")))?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            // Read inside the transaction, so a write that lands after it makes this one
            // conflict (serializable) rather than overwrite it.
            let current = txn.get(&self.ruleset_seq, &uid_bytes)?;
            let current = decode_seq(current.as_deref()).map_err(hs_kv::KvError::backend)?;
            if current != expected {
                return Ok(None);
            }
            self.rulesets
                .put(txn, &key, &value)
                .map_err(hs_kv::KvError::backend)?;
            let seq = txn.atomic_add(&self.ruleset_seq, &uid_bytes, 1)?;
            #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
            Ok(Some(seq as u64))
        })
        .map_err(StoreError::from)
    }

    async fn changed_seq(&self, user_id: &UserId) -> Result<u64, StoreError> {
        let snap = self.backend.snapshot();
        let bytes = snap
            .get(&self.ruleset_seq, user_id.as_bytes())
            .map_err(StoreError::from)?;
        decode_seq(bytes.as_deref())
    }
}

/// A change-seq counter's value (`0` when absent).
fn decode_seq(bytes: Option<&[u8]>) -> Result<u64, StoreError> {
    match bytes {
        Some(bytes) => {
            let arr: [u8; 8] = bytes
                .try_into()
                .map_err(|_| StoreError::Backend("expected an 8-byte counter".to_owned()))?;
            #[allow(clippy::cast_sign_loss, reason = "atomic_add never goes negative here")]
            Ok(i64::from_be_bytes(arr) as u64)
        }
        None => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_kv::memory::MemoryBackend;
    use ruma::user_id;

    #[tokio::test]
    async fn round_trips_through_a_memory_backend() {
        let store = TablesRulesetStore::open(MemoryBackend::new()).unwrap();
        let alice = user_id!("@alice:example.org");
        assert!(store.get_ruleset(alice).await.unwrap().is_none());

        let ruleset = super::super::default_ruleset(alice);
        store.set_ruleset(alice, &ruleset).await.unwrap();

        let read_back = store.get_ruleset(alice).await.unwrap().unwrap();
        assert!(
            read_back
                .get(crate::ruleset::RuleKind::Underride, ".m.rule.message")
                .is_some()
        );
    }

    /// The conditional write lands only at the change-seq it was given.
    #[tokio::test]
    async fn a_conditional_write_lands_only_at_the_seq_it_expects() {
        let store = TablesRulesetStore::open(MemoryBackend::new()).unwrap();
        let alice = user_id!("@alice:example.org");
        let mut ruleset = super::super::default_ruleset(alice);
        assert_eq!(
            store.set_ruleset_if(alice, &ruleset, 0).await.unwrap(),
            Some(1)
        );
        ruleset
            .set_enabled(
                crate::ruleset::RuleKind::Underride,
                ".m.rule.message",
                false,
            )
            .unwrap();
        assert_eq!(
            store.set_ruleset_if(alice, &ruleset, 0).await.unwrap(),
            None,
            "another write landed since 0"
        );
        let stored = store.get_ruleset(alice).await.unwrap().unwrap();
        assert!(
            stored
                .get(crate::ruleset::RuleKind::Underride, ".m.rule.message")
                .unwrap()
                .enabled(),
            "the refused write changed nothing"
        );
        assert_eq!(
            store.set_ruleset_if(alice, &ruleset, 1).await.unwrap(),
            Some(2)
        );
        assert_eq!(store.changed_seq(alice).await.unwrap(), 2);
    }
}
