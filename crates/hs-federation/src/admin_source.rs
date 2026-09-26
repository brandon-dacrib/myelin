//! `hs-admin`'s [`FederationSource`] over this crate's [`DestinationStore`]: what the admin
//! API's `federation.destinations.*` operations, the Federation page and the Overview's
//! federation panel read.
//!
//! A destination is every remote server this one has tried to reach; what is known about each
//! is the backoff bookkeeping the client keeps per destination, plus -- when a
//! [`crate::sender::FederationSender`] is attached with [`DestinationStoreSource::with_sender`]
//! -- how many PDUs are queued for it and not yet accepted. Without a sender the pending counts
//! are zero, which is then the truth: nothing is queued anywhere. EDUs are never sent yet (see
//! `crate::sender`), so the pending EDU count is always zero.

use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::model::AdminDestination;
use hs_admin::sources::{FederationSource, SourceError};

use crate::destination_store::{DestinationState, DestinationStore};
use crate::sender::FederationSender;

/// See the module docs.
pub struct DestinationStoreSource {
    destinations: Arc<dyn DestinationStore>,
    sender: Option<Arc<FederationSender>>,
}

impl DestinationStoreSource {
    #[must_use]
    pub fn new(destinations: Arc<dyn DestinationStore>) -> Self {
        Self {
            destinations,
            sender: None,
        }
    }

    /// Reports `sender`'s per-destination pending PDU counts alongside the backoff records. A
    /// destination with PDUs queued but no backoff record yet (nothing has been tried) is listed
    /// too, so an administrator sees where a queue is building before the first attempt.
    #[must_use]
    pub fn with_sender(mut self, sender: Arc<FederationSender>) -> Self {
        self.sender = Some(sender);
        self
    }

    fn pending_for(&self, server_name: &str) -> u64 {
        self.sender
            .as_ref()
            .map_or(0, |sender| sender.pending_pdus_for(server_name) as u64)
    }

    async fn all(&self) -> Vec<AdminDestination> {
        let mut rows: Vec<AdminDestination> = self
            .destinations
            .list()
            .await
            .iter()
            .map(|(name, state)| view(name, state, self.pending_for(name)))
            .collect();
        if let Some(sender) = &self.sender {
            for (name, pending) in sender.pending_by_destination() {
                if pending > 0 && !rows.iter().any(|row| row.server_name == name) {
                    rows.push(view(&name, &DestinationState::default(), pending as u64));
                }
            }
            rows.sort_by(|a, b| a.server_name.cmp(&b.server_name));
        }
        rows
    }
}

fn rfc3339(ms: Option<u64>) -> Option<String> {
    ms.map(|ms| hs_http::time::rfc3339_from_millis(i64::try_from(ms).unwrap_or(i64::MAX)))
}

fn view(server_name: &str, state: &DestinationState, pending_pdu_count: u64) -> AdminDestination {
    AdminDestination {
        server_name: server_name.to_owned(),
        last_successful_at: rfc3339(state.last_success_ms),
        failing_since: rfc3339(state.failing_since_ms),
        retry_last_at: rfc3339(state.last_attempt_ms),
        retry_interval_ms: state.retry_interval_ms(),
        pending_pdu_count,
        pending_edu_count: 0,
    }
}

#[async_trait]
impl FederationSource for DestinationStoreSource {
    async fn list_destinations(&self) -> Result<Vec<AdminDestination>, SourceError> {
        Ok(self.all().await)
    }

    async fn get_destination(
        &self,
        server_name: &str,
    ) -> Result<Option<AdminDestination>, SourceError> {
        // The store answers a default state for anything; "never tried" is the absence of a
        // record, which the list is the only way to see.
        Ok(self
            .all()
            .await
            .into_iter()
            .find(|row| row.server_name == server_name))
    }

    async fn reset_destination(&self, server_name: &str) -> Result<AdminDestination, SourceError> {
        if self.get_destination(server_name).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        self.destinations.reset(server_name).await;
        let state = self.destinations.get(server_name).await;
        Ok(view(server_name, &state, self.pending_for(server_name)))
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
