//! An in-memory [`super::RulesetStore`], for tests.

use std::collections::HashMap;
use std::sync::RwLock;

use ruma::UserId;
use ruma::push::Ruleset;

use super::RulesetStore;
use crate::error::StoreError;

/// One user's stored ruleset, paired with the change-seq it was last written at (see
/// [`RulesetStore::changed_seq`]).
#[derive(Debug, Clone)]
struct Row {
    ruleset: Ruleset,
    changed_seq: u64,
}

/// An in-memory `user_id -> Ruleset` map, plus a per-user change-seq counter.
#[derive(Debug, Default)]
pub struct InMemoryRulesetStore {
    rows: RwLock<HashMap<ruma::OwnedUserId, Row>>,
}

impl InMemoryRulesetStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl RulesetStore for InMemoryRulesetStore {
    async fn get_ruleset(&self, user_id: &UserId) -> Result<Option<Ruleset>, StoreError> {
        Ok(self
            .rows
            .read()
            .unwrap()
            .get(user_id)
            .map(|row| row.ruleset.clone()))
    }

    async fn set_ruleset(&self, user_id: &UserId, ruleset: &Ruleset) -> Result<u64, StoreError> {
        let mut rows = self.rows.write().unwrap();
        let next_seq = rows.get(user_id).map_or(0, |row| row.changed_seq) + 1;
        rows.insert(
            user_id.to_owned(),
            Row {
                ruleset: ruleset.clone(),
                changed_seq: next_seq,
            },
        );
        Ok(next_seq)
    }

    async fn changed_seq(&self, user_id: &UserId) -> Result<u64, StoreError> {
        Ok(self
            .rows
            .read()
            .unwrap()
            .get(user_id)
            .map_or(0, |row| row.changed_seq))
    }
}
