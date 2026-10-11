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
//!
//! With [`DestinationStoreSource::with_room_sharing`] it knows which rooms this server shares
//! with each destination ([`crate::room_sharing`]), which every row carries as
//! `shared_rooms_count` and which decides what may be forgotten (decision 0042):
//! `federation.destinations.forget` refuses a destination that shares a room unless forced,
//! and `federation.destinations.prune` -- and the background sweep `hs-cli` runs through
//! [`DestinationStoreSource::sweep`] -- forget what `hs_admin::federation::decide` says to.
//! Forgetting a destination drops the sender's queue and retry state for it
//! ([`FederationSender::forget_destination`]), the client's backoff record
//! ([`DestinationStore::forget`]) and its cached keys
//! ([`crate::keys::RemoteKeyCache::forget_server`]). One reading of the room sharing loads
//! every room, so a reading is kept for [`SHARING_CACHE_FOR`] and reused by the list; a forget
//! or a prune reads afresh.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hs_admin::federation::{
    AdminDestinationForgotten, AdminPruneReport, AdminRemoteServerKeys, AdminSigningKey,
    DestinationFacts, ForgetOutcome, PRUNE_ROOMS_INSPECTED, PruneDecision, PruneOptions,
    PruneReason, PruneRules, decide, report,
};
use hs_admin::model::AdminDestination;
use hs_admin::sources::{FederationSource, SourceError};

use crate::destination_store::{DestinationState, DestinationStore};
use crate::keys::{CachedServerKeys, DynRemoteKeyCache, OwnSigningKeys};
use crate::outbound_store::OutboundDestinationState;
use crate::room_sharing::{RoomSharing, Sharing};
use crate::sender::FederationSender;

/// How long one reading of the room sharing is reused by the destination list before it is
/// read again. A forget or a prune always reads afresh.
pub const SHARING_CACHE_FOR: Duration = Duration::from_secs(30);

/// See the module docs.
pub struct DestinationStoreSource {
    destinations: Arc<dyn DestinationStore>,
    sender: Option<Arc<FederationSender>>,
    keys: Option<(Arc<OwnSigningKeys>, Arc<DynRemoteKeyCache>)>,
    sharing: Option<(Arc<dyn RoomSharing>, String)>,
    sharing_cache: Mutex<Option<(Instant, Arc<Sharing>)>>,
}

impl DestinationStoreSource {
    #[must_use]
    pub fn new(destinations: Arc<dyn DestinationStore>) -> Self {
        Self {
            destinations,
            sender: None,
            keys: None,
            sharing: None,
            sharing_cache: Mutex::new(None),
        }
    }

    /// Knows which rooms `own_server_name` shares with each destination through `sharing` (see
    /// the module docs). Without it every row's `shared_rooms_count` is `None`, a forget needs
    /// `force`, and a prune is unavailable.
    #[must_use]
    pub fn with_room_sharing(
        mut self,
        sharing: Arc<dyn RoomSharing>,
        own_server_name: impl Into<String>,
    ) -> Self {
        self.sharing = Some((sharing, own_server_name.into()));
        self
    }

    /// The room sharing as of now (`fresh`) or as last read within [`SHARING_CACHE_FOR`];
    /// `Ok(None)` without a source.
    async fn sharing(&self, fresh: bool) -> Result<Option<Arc<Sharing>>, SourceError> {
        let Some((source, own)) = &self.sharing else {
            return Ok(None);
        };
        if !fresh
            && let Some((read_at, sharing)) = self
                .sharing_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            && read_at.elapsed() < SHARING_CACHE_FOR
        {
            return Ok(Some(Arc::clone(sharing)));
        }
        let rooms = source.shared_rooms().await.map_err(|reason| {
            SourceError::Unavailable(format!("cannot read which rooms are shared: {reason}"))
        })?;
        let sharing = Arc::new(Sharing::new(own, rooms));
        *self
            .sharing_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), Arc::clone(&sharing)));
        Ok(Some(sharing))
    }

    /// Drops everything held for `server_name`: the sender's queue and state, the client's
    /// record, the cached keys. `shared_rooms` is reported back; `reason` and `by` label the
    /// metric.
    async fn forget(
        &self,
        server_name: &str,
        shared_rooms: u64,
        reason: &'static str,
        by: &'static str,
    ) -> Result<AdminDestinationForgotten, SourceError> {
        let queue = match &self.sender {
            Some(sender) => sender.forget_destination(server_name).map_err(|error| {
                tracing::error!(server_name, %error, "cannot forget the outbound sender's queue");
                SourceError::Unavailable(format!(
                    "the outbound queue could not be dropped: {error}"
                ))
            })?,
            None => crate::outbound_store::ForgottenQueue::default(),
        };
        self.destinations.forget(server_name).await;
        let dropped_keys = match &self.keys {
            Some((_, cache)) => cache.forget_server(server_name),
            None => 0,
        };
        crate::metrics::record_destination_forgotten(reason, by);
        // One line per destination, whoever decided: the admin API's handlers log the request
        // and the sweep its summary, but the destination and the reason belong together.
        tracing::info!(
            destination = %server_name,
            reason,
            by,
            dropped_pdus = queue.pdus,
            dropped_edus = queue.edus,
            dropped_keys,
            was_catching_up = queue.was_catching_up,
            shared_rooms,
            "forgot a federation destination: its queue, backoff, catch-up mark and cached keys are gone"
        );
        Ok(AdminDestinationForgotten {
            server_name: server_name.to_owned(),
            dropped_pdu_count: queue.pdus as u64,
            dropped_edu_count: queue.edus as u64,
            dropped_key_count: dropped_keys as u64,
            was_catching_up: queue.was_catching_up,
            shared_rooms_count: shared_rooms,
        })
    }

    /// Every destination any of the sources knows: the client's backoff records, the sender's
    /// retry states, its catch-up marks, its workers' queues, and the store's queues (which
    /// another replica may be sending).
    fn names(
        &self,
        client_states: &BTreeMap<String, DestinationState>,
        outbound_states: &BTreeMap<String, OutboundDestinationState>,
        catch_up: &BTreeMap<String, u64>,
    ) -> BTreeSet<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        names.extend(client_states.keys().cloned());
        names.extend(outbound_states.keys().cloned());
        names.extend(catch_up.keys().cloned());
        if let Some(sender) = &self.sender {
            names.extend(
                sender
                    .pending_by_destination()
                    .into_iter()
                    .filter(|(_, pending)| *pending > 0)
                    .map(|(name, _)| name),
            );
            match sender.destinations_with_queues_in_store() {
                Ok(queued) => names.extend(queued),
                Err(error) => {
                    tracing::error!(%error, "cannot read which destinations the outbound store holds queues for");
                }
            }
        }
        names
    }

    /// The facts [`decide`] looks at, for every destination known, against `sharing`.
    async fn facts(&self, sharing: &Sharing) -> Vec<DestinationFacts> {
        let client_states: BTreeMap<String, DestinationState> =
            self.destinations.list().await.into_iter().collect();
        let outbound_states = self.outbound_states();
        let catch_up = self.catch_up_since();
        let names = self.names(&client_states, &outbound_states, &catch_up);
        let mut facts = Vec::with_capacity(names.len());
        for name in names {
            let default_client = DestinationState::default();
            let default_outbound = OutboundDestinationState::default();
            let client = client_states.get(&name).unwrap_or(&default_client);
            let outbound = outbound_states.get(&name).unwrap_or(&default_outbound);
            let (queued_pdus, queued_edus, rooms_behind) = match &self.sender {
                Some(sender) => {
                    let logged = |what: &str, error: crate::outbound_store::OutboundStoreError| {
                        tracing::error!(destination = %name, %error, "cannot read the outbound store's {what}");
                    };
                    let pdus = sender.queued_pdus_in_store(&name).unwrap_or_else(|e| {
                        logged("queue", e);
                        0
                    });
                    let edus = sender.durable_edus_in_store(&name).unwrap_or_else(|e| {
                        logged("durable EDUs", e);
                        0
                    });
                    let rooms = sender
                        .rooms_behind(&name, PRUNE_ROOMS_INSPECTED)
                        .unwrap_or_else(|e| {
                            logged("room positions", e);
                            Vec::new()
                        });
                    (pdus, edus, rooms)
                }
                None => (0, 0, Vec::new()),
            };
            let (current, left) =
                rooms_behind
                    .iter()
                    .fold((0u64, 0u64), |(current, left), room| {
                        if sharing.still_in(&room.room_id) {
                            (current + 1, left)
                        } else {
                            (current, left + 1)
                        }
                    });
            facts.push(DestinationFacts {
                shared_rooms: sharing.rooms_shared_with(&name),
                queued_pdus: queued_pdus as u64,
                queued_edus: queued_edus as u64,
                catching_up: catch_up.contains_key(&name),
                rooms_behind_current: current,
                rooms_behind_left: left,
                failing_since_ms: earliest(client.failing_since_ms, outbound.failing_since_ms),
                last_attempt_ms: latest(client.last_attempt_ms, outbound.last_attempt_ms),
                last_success_ms: latest(client.last_success_ms, outbound.last_success_ms),
                server_name: name,
            });
        }
        facts
    }

    /// Decides every destination against `rules` now, forgets the ones to forget unless
    /// `dry_run`, and reports. `by` labels the metric (`administrator` or `sweep`).
    async fn prune_with(
        &self,
        rules: PruneRules,
        dry_run: bool,
        by: &'static str,
    ) -> Result<AdminPruneReport, SourceError> {
        let Some(sharing) = self.sharing(true).await? else {
            return Err(SourceError::Unavailable(
                "this server cannot say which rooms it shares, so it cannot tell a destination \
                 it may forget from one it may not"
                    .to_owned(),
            ));
        };
        let now_ms = now_ms();
        let decisions: Vec<PruneDecision> = self
            .facts(&sharing)
            .await
            .iter()
            .map(|facts| decide(facts, &rules, now_ms))
            .collect();
        if !dry_run {
            for decision in decisions.iter().filter(|d| d.forget()) {
                let reason = match decision.reason {
                    PruneReason::Failing => "failing",
                    _ => "unused",
                };
                if let Err(error) = self.forget(&decision.server_name, 0, reason, by).await {
                    tracing::error!(destination = %decision.server_name, %error, "could not forget a destination the prune chose");
                }
            }
        }
        Ok(report(&decisions, dry_run))
    }

    /// The background sweep (`hs-cli`): forgets every destination [`decide`] says to under
    /// `rules`, and answers the report. Counted as `by="sweep"`.
    ///
    /// # Errors
    /// [`SourceError::Unavailable`] without a room-sharing source, or when it cannot be read.
    pub async fn sweep(&self, rules: PruneRules) -> Result<AdminPruneReport, SourceError> {
        self.prune_with(rules, false, "sweep").await
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

    /// PDUs waiting for `server_name`: this replica's worker's count when it has a worker for
    /// it (what the worker will send, counted as it was queued or resumed), else what the store
    /// holds (queued here for the replica that sends for it, or left by a previous run and not
    /// resumed yet). Never the store's count for a destination with a worker: the store may hold
    /// rows the worker has already sent and not yet removed, or that were written around it.
    fn pending_for(&self, server_name: &str) -> u64 {
        let Some(sender) = &self.sender else {
            return 0;
        };
        if sender.has_worker_for(server_name) {
            return sender.pending_pdus_for(server_name) as u64;
        }
        sender.queued_pdus_in_store(server_name).unwrap_or(0) as u64
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
        let sharing = match self.sharing(false).await {
            Ok(sharing) => sharing,
            Err(error) => {
                tracing::error!(%error, "cannot read which rooms are shared; the destination rows say so");
                None
            }
        };
        // One row per destination, from whichever of the sources knows it (`names`).
        let client_states: BTreeMap<String, DestinationState> =
            self.destinations.list().await.into_iter().collect();
        let outbound_states = self.outbound_states();
        let catch_up = self.catch_up_since();
        self.names(&client_states, &outbound_states, &catch_up)
            .into_iter()
            .map(|name| {
                let pending = self.pending_for(&name);
                let mut row = view(
                    &name,
                    client_states.get(&name),
                    outbound_states.get(&name),
                    pending,
                    catch_up.get(&name).copied(),
                );
                row.shared_rooms_count = sharing.as_ref().map(|s| s.rooms_shared_with(&name));
                row
            })
            .collect()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
        shared_rooms_count: None,
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
        let mut row = view(
            server_name,
            Some(&state),
            outbound.get(server_name),
            self.pending_for(server_name),
            self.catch_up_since().get(server_name).copied(),
        );
        row.shared_rooms_count = self
            .sharing(false)
            .await?
            .map(|s| s.rooms_shared_with(server_name));
        Ok(row)
    }

    async fn forget_destination(
        &self,
        server_name: &str,
        force: bool,
    ) -> Result<ForgetOutcome, SourceError> {
        if self.get_destination(server_name).await?.is_none() {
            return Err(SourceError::NotFound);
        }
        let shared = match self.sharing(true).await? {
            Some(sharing) => sharing.rooms_shared_with(server_name),
            None if force => 0,
            None => {
                return Err(SourceError::Unavailable(
                    "this server cannot say which rooms it shares with the destination; \
                     forget it with force=true if you are sure"
                        .to_owned(),
                ));
            }
        };
        if shared > 0 && !force {
            return Ok(ForgetOutcome::SharesRooms { rooms: shared });
        }
        self.forget(server_name, shared, "administrator", "administrator")
            .await
            .map(ForgetOutcome::Forgotten)
    }

    async fn prune_destinations(
        &self,
        options: PruneOptions,
    ) -> Result<AdminPruneReport, SourceError> {
        let rules = PruneRules {
            idle_for: Duration::ZERO,
            failing_for: options.failing_for,
        };
        self.prune_with(rules, options.dry_run, "administrator")
            .await
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

    fn client(client_store: Arc<InMemoryDestinationStore>) -> Arc<FederationClient> {
        Arc::new(FederationClient::new(
            "us.example",
            hs_model::signing::SigningKeyPair::generate("a_1"),
            ClientConfig::default(),
            client_store,
            Arc::new(Nothing),
            Arc::new(Nothing),
            Arc::new(Nothing),
        ))
    }

    /// A row's pending count is the worker's when this replica has a worker for the destination,
    /// even when the store holds more rows for it (written around the worker, or not yet removed
    /// after a send); the store's count is only for a destination without a worker here (left by
    /// a previous run and not resumed, or queued for the replica that sends for it).
    #[tokio::test]
    async fn pending_is_the_workers_count_with_a_worker_and_the_stores_without_one() {
        let client_store = Arc::new(InMemoryDestinationStore::new());
        let outbound: Arc<dyn OutboundStore> = Arc::new(InMemoryOutboundStore::new());
        let sender = Arc::new(FederationSender::with_store(
            client(client_store.clone()),
            "us.example",
            SenderConfig::default(),
            outbound.clone(),
        ));
        let source = DestinationStoreSource::new(client_store.clone()).with_sender(sender.clone());
        let pdu = serde_json::json!({"type": "m.room.message", "room_id": "!r:us.example"});

        // A worker for `worked` counts the one PDU queued through the sender; two more rows
        // written straight to the store (another writer) are not its to count.
        sender.enqueue_pdu(["worked.example".to_owned()], pdu.clone());
        assert!(sender.has_worker_for("worked.example"));
        assert_eq!(sender.pending_pdus_for("worked.example"), 1);
        for _ in 0..2 {
            outbound
                .enqueue(
                    &["worked.example".to_owned()],
                    Some("!r:us.example"),
                    &pdu,
                    100,
                )
                .unwrap();
        }
        assert_eq!(sender.queued_pdus_in_store("worked.example").unwrap(), 3);
        assert_eq!(source.pending_for("worked.example"), 1);

        // No worker for `stored`: the store's rows are what waits.
        outbound
            .enqueue(
                &["stored.example".to_owned()],
                Some("!r:us.example"),
                &pdu,
                100,
            )
            .unwrap();
        assert!(!sender.has_worker_for("stored.example"));
        assert_eq!(source.pending_for("stored.example"), 1);

        let rows = source.list_destinations().await.unwrap();
        let pending = |name: &str| {
            rows.iter()
                .find(|r| r.server_name == name)
                .unwrap_or_else(|| panic!("{name} missing from {rows:?}"))
                .pending_pdu_count
        };
        assert_eq!(pending("worked.example"), 1);
        assert_eq!(pending("stored.example"), 1);
        sender.shutdown();
    }

    /// Forgetting and pruning over the real stores (decision 0042): a destination sharing a
    /// room is refused unless forced; forgetting drops the sender's queue, the client's record
    /// and the cached keys; a prune keeps relationships and forgets state, dry run first.
    #[tokio::test]
    async fn forgetting_and_pruning_drop_state_and_keep_relationships() {
        use crate::room_sharing::{FixedRoomSharing, SharedRoom};
        use hs_admin::federation::ForgetOutcome;

        let client_store = Arc::new(InMemoryDestinationStore::new());
        let outbound: Arc<dyn OutboundStore> = Arc::new(InMemoryOutboundStore::new());
        let sender = Arc::new(FederationSender::with_store(
            client(client_store.clone()),
            "us.example",
            SenderConfig::default(),
            outbound.clone(),
        ));
        let sharing = Arc::new(FixedRoomSharing::new());
        let room = |id: &str, servers: &[&str]| SharedRoom {
            room_id: id.to_owned(),
            servers: servers.iter().map(|s| (*s).to_owned()).collect(),
        };
        sharing.set(vec![
            room("!shared:us.example", &["us.example", "friend.example"]),
            // Left: only they are in it now.
            room("!left:them.example", &["stale.example", "healthy.example"]),
        ]);
        let source = DestinationStoreSource::new(client_store.clone())
            .with_sender(sender.clone())
            .with_room_sharing(sharing.clone(), "us.example");

        // friend shares a room and is healthy; lonely failed once and shares nothing; stale
        // and healthy each have our leave queued for the room we left, stale failing.
        client_store.record_success("friend.example").await;
        client_store.record_failure("lonely.example", 60_000).await;
        let leave = serde_json::json!({"type": "m.room.member", "room_id": "!left:them.example"});
        outbound
            .enqueue(
                &["stale.example".to_owned(), "healthy.example".to_owned()],
                Some("!left:them.example"),
                &leave,
                100,
            )
            .unwrap();
        outbound
            .record_failure("stale.example", "HTTP 502", u64::MAX / 2)
            .unwrap();
        // kicked was removed from the shared room; its kick is queued for a room we are in.
        outbound
            .enqueue(
                &["kicked.example".to_owned()],
                Some("!shared:us.example"),
                &serde_json::json!({"type": "m.room.member"}),
                100,
            )
            .unwrap();

        let rows = source.list_destinations().await.unwrap();
        let shared = |name: &str| {
            rows.iter()
                .find(|r| r.server_name == name)
                .unwrap_or_else(|| panic!("{name} missing from {rows:?}"))
                .shared_rooms_count
        };
        assert_eq!(shared("friend.example"), Some(1));
        assert_eq!(shared("lonely.example"), Some(0));
        assert_eq!(shared("stale.example"), Some(0));

        assert_eq!(
            source
                .forget_destination("friend.example", false)
                .await
                .unwrap(),
            ForgetOutcome::SharesRooms { rooms: 1 }
        );
        assert!(matches!(
            source.forget_destination("never.example", false).await,
            Err(SourceError::NotFound)
        ));

        let dry = source
            .prune_destinations(hs_admin::federation::PruneOptions {
                dry_run: true,
                failing_for: Some(Duration::ZERO),
            })
            .await
            .unwrap();
        assert!(dry.dry_run);
        let forgotten: Vec<&str> = dry
            .forgotten
            .servers
            .iter()
            .map(|e| e.server_name.as_str())
            .collect();
        assert_eq!(
            forgotten,
            vec!["lonely.example", "stale.example"],
            "{dry:?}"
        );
        assert_eq!(dry.forgotten.by_reason["unused"], 1);
        assert_eq!(dry.forgotten.by_reason["failing"], 1);
        let kept: BTreeMap<&str, &str> = dry
            .kept
            .servers
            .iter()
            .map(|e| (e.server_name.as_str(), e.reason.as_str()))
            .collect();
        assert_eq!(kept["friend.example"], "shares_rooms");
        assert_eq!(kept["healthy.example"], "queued_not_failing");
        assert_eq!(kept["kicked.example"], "queued_for_current_rooms");
        assert_eq!(
            source.list_destinations().await.unwrap().len(),
            5,
            "a dry run forgets nothing"
        );

        let before = crate::metrics::destinations_forgotten("failing", "administrator");
        let real = source
            .prune_destinations(hs_admin::federation::PruneOptions {
                dry_run: false,
                failing_for: Some(Duration::ZERO),
            })
            .await
            .unwrap();
        assert_eq!(real.forgotten.count, 2);
        let names: Vec<String> = source
            .list_destinations()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.server_name)
            .collect();
        assert_eq!(
            names,
            vec!["friend.example", "healthy.example", "kicked.example"]
        );
        assert_eq!(
            outbound.queue_len("stale.example").unwrap(),
            0,
            "its queue went with it"
        );
        assert!(outbound.state("stale.example").unwrap().is_none());
        assert_eq!(
            outbound.queue_len("healthy.example").unwrap(),
            1,
            "the other's stays"
        );
        assert_eq!(
            crate::metrics::destinations_forgotten("failing", "administrator"),
            before + 1
        );

        // Forced, a relationship goes too.
        let forced = match source
            .forget_destination("friend.example", true)
            .await
            .unwrap()
        {
            ForgetOutcome::Forgotten(forgotten) => forgotten,
            other => panic!("{other:?}"),
        };
        assert_eq!(forced.shared_rooms_count, 1);
        assert!(
            source
                .get_destination("friend.example")
                .await
                .unwrap()
                .is_none()
        );

        // Without a room-sharing source, a forget needs force.
        let blind = DestinationStoreSource::new(client_store.clone());
        client_store.record_failure("blind.example", 60_000).await;
        assert!(matches!(
            blind.forget_destination("blind.example", false).await,
            Err(SourceError::Unavailable(_))
        ));
        assert!(matches!(
            blind.forget_destination("blind.example", true).await,
            Ok(ForgetOutcome::Forgotten(_))
        ));
    }

    /// The sender's persisted retry state is part of the row: a destination the client has
    /// no record of but the sender failed to send to is listed as failing, and a reset clears
    /// the sender's backoff along with the client's.
    #[tokio::test]
    async fn the_senders_persisted_retry_state_is_merged_into_the_row_and_reset_with_it() {
        let client_store = Arc::new(InMemoryDestinationStore::new());
        let client = client(client_store.clone());
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
