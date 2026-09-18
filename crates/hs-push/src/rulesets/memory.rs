//! An in-memory [`super::RulesetStore`], for tests.

use std::collections::HashMap;
use std::sync::RwLock;

use ruma::UserId;
use ruma::push::Ruleset;

use super::RulesetStore;
use crate::error::StoreError;

/// An in-memory `user_id -> Ruleset` map.
#[derive(Debug, Default)]
pub struct InMemoryRulesetStore {
    rows: RwLock<HashMap<ruma::OwnedUserId, Ruleset>>,
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
        Ok(self.rows.read().unwrap().get(user_id).cloned())
    }

    async fn set_ruleset(&self, user_id: &UserId, ruleset: &Ruleset) -> Result<(), StoreError> {
        self.rows
            .write()
            .unwrap()
            .insert(user_id.to_owned(), ruleset.clone());
        Ok(())
    }
}
