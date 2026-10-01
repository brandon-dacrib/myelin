//! `hs-admin`'s [`FederationSource`] over this crate's [`DestinationStore`]: what the admin
//! API's `federation.destinations.*` operations, the Federation page and the Overview's
//! federation panel read.
//!
//! A destination is every remote server this one has tried to reach; what is known about each
//! is the backoff bookkeeping the client keeps per destination (`crate::destination_store`,
//! connection-level, every outbound call), plus -- when a [`crate::sender::FederationSender`]
//! is attached with [`DestinationStoreSource::with_sender`] -- how many PDUs are queued for it
//! and not yet accepted, and the sender's own persisted retry state for it
//! (`crate::outbound_store::OutboundDestinationState`: how the head transaction's retrying is
//! going). The two records are merged into one row: a destination is failing if either says
//! so, since its earliest failure; its last attempt and last success are the latest either
//! knows; and a reset clears both. Without a sender the pending counts are zero, which is then
//! the truth: nothing is queued anywhere. EDUs are never sent yet (see `crate::sender`), so the
//! pending EDU count is always zero.
//!
//! With [`DestinationStoreSource::with_keys`] it also serves the `federation.keys.*`
//! operations: this server's own signing keys ([`crate::keys::OwnSigningKeys`]) and what the
//! key cache ([`crate::keys::RemoteKeyCache`], the one `X-Matrix` verification reads) holds for
//! another server, which a refresh fetches again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use hs_admin::federation::{AdminRemoteServerKeys, AdminSigningKey};
use hs_admin::model::AdminDestination;
use hs_admin::sources::{FederationSource, SourceError};

use crate::destination_store::{DestinationState, DestinationStore};
use crate::keys::{CachedServerKeys, DynRemoteKeyCache, OwnSigningKeys};
use crate::outbound_store::OutboundDestinationState;
use crate::sender::FederationSender;

/// See the module docs.
pub struct DestinationStoreSource {
    destinations: Arc<dyn DestinationStore>,
    sender: Option<Arc<FederationSender>>,
    keys: Option<(Arc<OwnSigningKeys>, Arc<DynRemoteKeyCache>)>,
}

impl DestinationStoreSource {
    #[must_use]
    pub fn new(destinations: Arc<dyn DestinationStore>) -> Self {
        Self {
            destinations,
            sender: None,
            keys: None,
        }
    }

    /// Serves `own` and `cache` for the `federation.keys.*` operations (see the module docs).
    #[must_use]
    pub fn with_keys(mut self, own: Arc<OwnSigningKeys>, cache: Arc<DynRemoteKeyCache>) -> Self {
        self.keys = Some((own, cache));
        self
    }

    #[allow(clippy::type_complexity)]
    fn keys(&self) -> Result<&(Arc<OwnSigningKeys>, Arc<DynRemoteKeyCache>), SourceError> {
        self.keys.as_ref().ok_or_else(|| {
            SourceError::Unavailable("the signing keys are not wired into this source".to_owned())
        })
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

    /// The sender's persisted retry states, by destination; empty without a sender, or when
    /// its store cannot be read (logged: the client's records are still shown then).
    fn outbound_states(&self) -> BTreeMap<String, OutboundDestinationState> {
        let Some(sender) = &self.sender else {
            return BTreeMap::new();
        };
        match sender.destination_states() {
            Ok(states) => states.into_iter().collect(),
            Err(error) => {
                tracing::error!(%error, "cannot read the outbound sender's retry states");
                BTreeMap::new()
            }
        }
    }

    /// When each destination in catch-up mode entered it, by destination; empty without a
    /// sender, or when its store cannot be read (logged).
    fn catch_up_since(&self) -> BTreeMap<String, u64> {
        let Some(sender) = &self.sender else {
            return BTreeMap::new();
        };
        match sender.catch_up_marks() {
            Ok(marks) => marks
                .into_iter()
                .map(|(name, mark)| (name, mark.since_ms))
                .collect(),
            Err(error) => {
                tracing::error!(%error, "cannot read the outbound sender's catch-up marks");
                BTreeMap::new()
            }
        }
    }

    async fn all(&self) -> Vec<AdminDestination> {
        // One row per destination, from whichever of the four sources knows it: the client's
        // backoff records, the sender's retry states, its catch-up marks, its queues.
        let mut names: BTreeSet<String> = BTreeSet::new();
        let client_states: BTreeMap<String, DestinationState> =
            self.destinations.list().await.into_iter().collect();
        names.extend(client_states.keys().cloned());
        let outbound_states = self.outbound_states();
        names.extend(outbound_states.keys().cloned());
        let catch_up = self.catch_up_since();
        names.extend(catch_up.keys().cloned());
        if let Some(sender) = &self.sender {
            names.extend(
                sender
                    .pending_by_destination()
                    .into_iter()
                    .filter(|(_, pending)| *pending > 0)
                    .map(|(name, _)| name),
            );
        }
        names
            .into_iter()
            .map(|name| {
                let pending = self.pending_for(&name);
                view(
                    &name,
                    client_states.get(&name),
                    outbound_states.get(&name),
                    pending,
                    catch_up.get(&name).copied(),
                )
            })
            .collect()
    }
}

fn rfc3339(ms: Option<u64>) -> Option<String> {
    ms.map(|ms| hs_http::time::rfc3339_from_millis(i64::try_from(ms).unwrap_or(i64::MAX)))
}

fn earliest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn latest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// One admin row from what the client and the sender each know about a destination (see the
/// module docs for how the two are merged).
fn view(
    server_name: &str,
    client: Option<&DestinationState>,
    outbound: Option<&OutboundDestinationState>,
    pending_pdu_count: u64,
    catch_up_since_ms: Option<u64>,
) -> AdminDestination {
    let default_client = DestinationState::default();
    let default_outbound = OutboundDestinationState::default();
    let client = client.unwrap_or(&default_client);
    let outbound = outbound.unwrap_or(&default_outbound);
    // The interval that is still running, if both are: whichever ends later.
    let retry_interval_ms = match (
        (client.retry_at_ms, client.retry_interval_ms()),
        (outbound.next_attempt_ms, outbound.retry_interval_ms()),
    ) {
        ((Some(client_at), Some(client_ms)), (Some(outbound_at), Some(outbound_ms))) => {
            Some(if client_at >= outbound_at {
                client_ms
            } else {
                outbound_ms
            })
        }
        ((_, client_ms), (_, outbound_ms)) => client_ms.or(outbound_ms),
    };
    AdminDestination {
        server_name: server_name.to_owned(),
        last_successful_at: rfc3339(latest(client.last_success_ms, outbound.last_success_ms)),
        failing_since: rfc3339(earliest(client.failing_since_ms, outbound.failing_since_ms)),
        retry_last_at: rfc3339(latest(client.last_attempt_ms, outbound.last_attempt_ms)),
        retry_interval_ms,
        pending_pdu_count,
        pending_edu_count: 0,
        catch_up_since: rfc3339(catch_up_since_ms),
    }
}

/// The admin view of what the cache holds for one server.
fn remote_view(cached: CachedServerKeys) -> AdminRemoteServerKeys {
    AdminRemoteServerKeys {
        server_name: cached.server_name,
        keys: cached
            .keys
            .into_iter()
            .map(|key| AdminSigningKey {
                algorithm: key
                    .key_id
                    .split_once(':')
                    .map_or_else(|| key.key_id.clone(), |(algorithm, _)| algorithm.to_owned()),
                key_id: key.key_id,
                public_key: key.public_key,
                valid_until_at: rfc3339(Some(key.valid_until_ts)),
                old: key.old,
            })
            .collect(),
        cached_at: rfc3339(cached.fetched_at_ms),
    }
}

#[async_trait]
impl FederationSource for DestinationStoreSource {
    async fn own_keys(&self) -> Result<Vec<AdminSigningKey>, SourceError> {
        let (own, _) = self.keys()?;
        let mut keys: Vec<AdminSigningKey> = own
            .all()
            .iter()
            .map(|key| AdminSigningKey {
                key_id: key.key_id(),
                algorithm: hs_model::signing::ALGORITHM.to_owned(),
                public_key: key.verifying_key_base64(),
                valid_until_at: None,
                old: false,
            })
            .collect();
        keys.sort_by(|a, b| a.key_id.cmp(&b.key_id));
        Ok(keys)
    }

    async fn remote_keys(
        &self,
        server_name: &str,
    ) -> Result<Option<AdminRemoteServerKeys>, SourceError> {
        let (_, cache) = self.keys()?;
        Ok(cache.cached_keys(server_name).map(remote_view))
    }

    async fn refresh_remote_keys(
        &self,
        server_name: &str,
    ) -> Result<AdminRemoteServerKeys, SourceError> {
        let (_, cache) = self.keys()?;
        cache
            .refetch(server_name)
            .await
            .map(remote_view)
            .map_err(|error| SourceError::Unavailable(error.to_string()))
    }

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
        if let Some(sender) = &self.sender
            && let Err(error) = sender.reset_destination(server_name)
        {
            tracing::error!(server_name, %error, "cannot reset the outbound sender's backoff");
            return Err(SourceError::Unavailable(format!(
                "the outbound queue's backoff could not be reset: {error}"
            )));
        }
        let state = self.destinations.get(server_name).await;
        let outbound = self.outbound_states();
        Ok(view(
            server_name,
            Some(&state),
            outbound.get(server_name),
            self.pending_for(server_name),
            self.catch_up_since().get(server_name).copied(),
        ))
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

    /// The sender's persisted retry state is part of the row: a destination the client has
    /// no record of but the sender failed to send to is listed as failing, and a reset clears
    /// the sender's backoff along with the client's.
    #[tokio::test]
    async fn the_senders_persisted_retry_state_is_merged_into_the_row_and_reset_with_it() {
        use crate::client::{ClientConfig, FederationClient};
        use crate::discovery::{AddrResolver, SrvResolver, WellKnownFetcher, WellKnownOutcome};
        use crate::outbound_store::{InMemoryOutboundStore, OutboundStore};
        use crate::sender::{FederationSender, SenderConfig};
        use std::net::IpAddr;
        use std::time::Duration;

        struct Nothing;
        #[async_trait]
        impl AddrResolver for Nothing {
            async fn resolve_addr(&self, _hostname: &str) -> Vec<IpAddr> {
                Vec::new()
            }
        }
        #[async_trait]
        impl SrvResolver for Nothing {
            async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
                Vec::new()
            }
        }
        #[async_trait]
        impl WellKnownFetcher for Nothing {
            async fn fetch(&self, _hostname: &str) -> WellKnownOutcome {
                WellKnownOutcome::Absent {
                    cache_for: Duration::from_secs(60),
                }
            }
        }

        let client_store = Arc::new(InMemoryDestinationStore::new());
        let client = Arc::new(FederationClient::new(
            "us.example",
            hs_model::signing::SigningKeyPair::generate("a_1"),
            ClientConfig::default(),
            client_store.clone(),
            Arc::new(Nothing),
            Arc::new(Nothing),
            Arc::new(Nothing),
        ));
        let outbound: Arc<dyn OutboundStore> = Arc::new(InMemoryOutboundStore::new());
        // What a previous run left: the sender could not deliver to `queued.example`.
        outbound
            .record_failure("queued.example", "HTTP 502", u64::MAX / 2)
            .unwrap();
        // And the client knows `both.example` failed at connect, the sender that its
        // transaction failed.
        client_store.record_failure("both.example", 60_000).await;
        outbound
            .record_failure("both.example", "HTTP 500", u64::MAX / 2)
            .unwrap();
        // And `catching.example` overflowed its queue: it is in catch-up mode.
        outbound
            .mark_catch_up(
                "catching.example",
                crate::outbound_store::CATCH_UP_REQUESTED,
            )
            .unwrap();
        let sender = Arc::new(FederationSender::with_store(
            client,
            "us.example",
            SenderConfig::default(),
            outbound.clone(),
        ));
        let source = DestinationStoreSource::new(client_store.clone()).with_sender(sender);

        let all = source.list_destinations().await.unwrap();
        assert_eq!(
            all.iter()
                .map(|row| row.server_name.as_str())
                .collect::<Vec<_>>(),
            vec!["both.example", "catching.example", "queued.example"]
        );
        let catching = &all[1];
        assert!(catching.catch_up_since.is_some(), "{catching:?}");
        assert!(all[0].catch_up_since.is_none() && all[2].catch_up_since.is_none());
        let queued = source
            .get_destination("queued.example")
            .await
            .unwrap()
            .unwrap();
        assert!(queued.failing_since.is_some(), "{queued:?}");
        assert!(queued.retry_last_at.is_some());
        assert!(queued.retry_interval_ms.is_some_and(|ms| ms > 0));
        let both = source
            .get_destination("both.example")
            .await
            .unwrap()
            .unwrap();
        assert!(both.failing_since.is_some(), "{both:?}");

        let reset = source.reset_destination("queued.example").await.unwrap();
        assert!(reset.failing_since.is_none(), "{reset:?}");
        assert!(reset.retry_interval_ms.is_none());
        let state = outbound.state("queued.example").unwrap().unwrap();
        assert_eq!(state.failures, 0);
        assert!(state.next_attempt_ms.is_none());
        assert_eq!(
            state.last_error.as_deref(),
            Some("HTTP 502"),
            "the last error is kept for the operator"
        );
        let reset = source.reset_destination("both.example").await.unwrap();
        assert!(reset.failing_since.is_none(), "{reset:?}");
        assert!(
            client_store
                .get("both.example")
                .await
                .is_ready(u64::MAX / 2)
        );
    }
}
