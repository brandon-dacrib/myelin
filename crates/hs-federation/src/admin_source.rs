//! `hs-admin`'s [`FederationSource`] over this crate's [`DestinationStore`]: what the admin
//! API's `federation.destinations.*` operations, the Federation page and the Overview's
//! federation panel read.
//!
//! A destination is every remote server this one has tried to reach; what is known about each
//! is the backoff bookkeeping the client keeps per destination. There is no outbound queue yet
//! -- nothing on this server sends events to other servers on its own -- so the pending counts
//! are zero, and that is the truth rather than a placeholder.

use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::model::AdminDestination;
use hs_admin::sources::{FederationSource, SourceError};

use crate::destination_store::{DestinationState, DestinationStore};

/// See the module docs.
pub struct DestinationStoreSource {
    destinations: Arc<dyn DestinationStore>,
}

impl DestinationStoreSource {
    #[must_use]
    pub fn new(destinations: Arc<dyn DestinationStore>) -> Self {
        Self { destinations }
    }
}

fn rfc3339(ms: Option<u64>) -> Option<String> {
    ms.map(|ms| hs_http::time::rfc3339_from_millis(i64::try_from(ms).unwrap_or(i64::MAX)))
}

fn view(server_name: &str, state: &DestinationState) -> AdminDestination {
    AdminDestination {
        server_name: server_name.to_owned(),
        last_successful_at: rfc3339(state.last_success_ms),
        failing_since: rfc3339(state.failing_since_ms),
        retry_last_at: rfc3339(state.last_attempt_ms),
        retry_interval_ms: state.retry_interval_ms(),
        pending_pdu_count: 0,
        pending_edu_count: 0,
    }
}

#[async_trait]
impl FederationSource for DestinationStoreSource {
    async fn list_destinations(&self) -> Result<Vec<AdminDestination>, SourceError> {
        Ok(self
            .destinations
            .list()
            .await
            .iter()
            .map(|(name, state)| view(name, state))
            .collect())
    }

    async fn get_destination(
        &self,
        server_name: &str,
    ) -> Result<Option<AdminDestination>, SourceError> {
        // The store answers a default state for anything; "never tried" is the absence of a
        // record, which the list is the only way to see.
        Ok(self
            .destinations
            .list()
            .await
            .iter()
            .find(|(name, _)| name == server_name)
            .map(|(name, state)| view(name, state)))
    }

    async fn reset_destination(&self, server_name: &str) -> Result<AdminDestination, SourceError> {
        if self.get_destination(server_name).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        self.destinations.reset(server_name).await;
        let state = self.destinations.get(server_name).await;
        Ok(view(server_name, &state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination_store::InMemoryDestinationStore;

    #[tokio::test]
    async fn a_failing_destination_is_reported_as_such_and_a_reset_clears_it() {
        let store = Arc::new(InMemoryDestinationStore::new());
        store.record_success("good.example").await;
        store.record_failure("bad.example", 60_000).await;
        store.record_failure("bad.example", 60_000).await;
        let source = DestinationStoreSource::new(store.clone());

        let all = source.list_destinations().await.unwrap();
        assert_eq!(all.len(), 2);
        let bad = source
            .get_destination("bad.example")
            .await
            .unwrap()
            .unwrap();
        assert!(bad.failing_since.is_some());
        assert!(bad.retry_last_at.is_some());
        assert!(bad.retry_interval_ms.is_some_and(|ms| ms > 0), "{bad:?}");
        assert!(bad.last_successful_at.is_none());
        let good = source
            .get_destination("good.example")
            .await
            .unwrap()
            .unwrap();
        assert!(good.failing_since.is_none());
        assert!(good.last_successful_at.is_some());
        assert!(
            source
                .get_destination("never.example")
                .await
                .unwrap()
                .is_none()
        );

        let reset = source.reset_destination("bad.example").await.unwrap();
        assert!(reset.failing_since.is_none());
        assert!(reset.retry_interval_ms.is_none());
        assert!(store.get("bad.example").await.is_ready(u64::MAX / 2));
        assert!(matches!(
            source.reset_destination("never.example").await.unwrap_err(),
            SourceError::NotFound
        ));
    }
}
