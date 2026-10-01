//! `hs_room::backfill::Backfill` over the federation client: how `GET /messages` reaches the
//! history of a room hosted elsewhere from before this server's user joined it.
//!
//! The same meeting point as [`crate::remote_join`]: `hs-room` serves `/messages` and knows when
//! a page has reached the oldest event it holds with the room's history continuing before it
//! (`RoomActor::history_before_oldest`); `hs-federation` speaks to other servers; this module
//! joins them. One call is one batch: `GET /_matrix/federation/v1/backfill/{roomId}` against a
//! server in the room, walking back from the oldest held event
//! (`RoomActor::backfill_anchor`), every PDU verified exactly as an inbound `/send` PDU is
//! (`hs_federation::inbound::verify_pdu` -- hashes, and the sender's server's signature), then
//! placed by `RoomActor::accept_history` below everything held.
//!
//! The same endpoint the inbound side already used for a different purpose:
//! `hs_federation::backfill::resolve_missing_ancestors` fetches the missing *ancestors* of an
//! event that arrived over `/send`, so that event can be authorized. This is the other trigger
//! for the same fetch -- a client reading backwards -- and it goes through the room actor's
//! history path rather than the ancestor-resolution one, because the events are not there to
//! authorize something newer; they are the history itself.
//!
//! The second kind of fetch, [`hs_room::backfill::Backfill::fill_gap`], is the history between a
//! leave and a rejoin through another server: a gap in the *middle* of the timeline
//! (`hs_room::actor::gaps`). The same endpoint, asked from the events the gap lacks
//! (`RoomActor::gap_anchor`), the same verification, the same placement.
//!
//! # The state at the batch
//! Before a batch is placed, the server that sent it is asked for the state at its oldest event
//! (`RoomActor::plan_history` says which): `GET /state_ids/{roomId}?event_id=`, then every event
//! that names -- the state, its auth chain, and any `auth_events` of the batch -- that this
//! server does not hold, by `GET /event/{eventId}`, each verified like any inbound PDU. When
//! more than a tenth of what it names is missing (Synapse's rule for the same choice), or more
//! than [`MAX_EVENT_FETCHES`], one `GET /state/{roomId}` fetches it all instead; `/state` is
//! also the fallback when `/state_ids` fails. The room actor derives the state at every later
//! event of the batch from that, and authorizes each event at its position
//! (`hs_room::actor::history`). When neither request is answered, the batch is placed with the
//! state walked back from what is held, as before.
//!
//! # What an operator sees
//! Every batch is logged at `info` (room, server, kind, how many events came, how many were
//! placed and rejected, and whether the state was fetched or walked) and counted:
//! `hs_room_backfilled_events_total{kind}` (events placed; `kind` is `before_oldest` or
//! `rejoin_gap`), `hs_room_backfill_batches_total{kind,outcome}` (batches placed, `outcome`
//! `state_fetched` or `state_walked`) and `hs_room_backfill_rejected_events_total{kind,outcome}`
//! (events refused by authorization, `outcome` `rejected_auth`), registered by
//! [`register_metrics`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use hs_federation::client::FederationClient;
use hs_federation::inbound::verify_pdu;
use hs_federation::keys::DynRemoteKeyCache;
use hs_kv::KvBackend;
use hs_model::Event;
use hs_room::RoomError;
use hs_room::actor::RoomActorHandle;
use hs_room::backfill::{FetchedState, HistoryKind, HistoryOutcome, HistoryPlan};
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use ruma::{OwnedEventId, OwnedRoomId, RoomId, RoomVersionId};
use serde_json::Value;

/// How many events one `/backfill` request asks for. The most this server's own side of that
/// endpoint will answer (`hs_federation::transport::read_routes`' clamp), and Synapse's number
/// too, which is the one Complement's `TestMessagesOverFederation` is written around: a
/// `/messages` page of more than this comes back short and with an `end`, and the next page
/// fetches the next batch.
pub const BATCH: usize = 100;

/// The most `GET /event/{eventId}` requests one state fetch makes. Past this, one `GET /state`
/// carries everything in a single answer.
pub const MAX_EVENT_FETCHES: usize = 50;

/// How many `GET /event/{eventId}` requests one state fetch has in flight at once.
const EVENT_FETCH_CONCURRENCY: usize = 8;

/// The `kind` label of `hs_room_backfilled_events_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct BackfilledLabels {
    kind: &'static str,
}

/// The labels of `hs_room_backfill_batches_total` and `hs_room_backfill_rejected_events_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct OutcomeLabels {
    kind: &'static str,
    outcome: &'static str,
}

/// Process-wide, like the other counters registered into each server's registry: a counter is
/// only an atomic.
static BACKFILLED: LazyLock<Family<BackfilledLabels, Counter>> = LazyLock::new(Family::default);
static BATCHES: LazyLock<Family<OutcomeLabels, Counter>> = LazyLock::new(Family::default);
static REJECTED: LazyLock<Family<OutcomeLabels, Counter>> = LazyLock::new(Family::default);

fn count(kind: HistoryKind, outcome: &HistoryOutcome) {
    let kind_label = kind.label();
    BACKFILLED
        .get_or_create(&BackfilledLabels { kind: kind_label })
        .inc_by(u64::try_from(outcome.added).unwrap_or(u64::MAX));
    BATCHES
        .get_or_create(&OutcomeLabels {
            kind: kind_label,
            outcome: outcome.state.label(),
        })
        .inc();
    if outcome.rejected > 0 {
        REJECTED
            .get_or_create(&OutcomeLabels {
                kind: kind_label,
                outcome: "rejected_auth",
            })
            .inc_by(u64::try_from(outcome.rejected).unwrap_or(u64::MAX));
    }
}

/// Registers the backfill counters into `registry`: `hs_room_backfilled_events_total{kind}`
/// (events fetched from another server and placed in a room's timeline as history, by where
/// they went -- `before_oldest`, before the oldest event held, a room joined elsewhere, or
/// `rejoin_gap`, the history between a leave and a rejoin), `hs_room_backfill_batches_total
/// {kind,outcome}` (batches placed, by whether the state at them was `state_fetched` from the
/// server that sent them or `state_walked` back from what is held) and
/// `hs_room_backfill_rejected_events_total{kind,outcome}` (backfilled events not placed because
/// authorization refused them at their position, `outcome` `rejected_auth`).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_room_backfilled_events",
        "Events fetched from another server and placed in a room's timeline as history, by \
         kind: before_oldest (before the oldest event held), rejoin_gap (between a leave and a \
         rejoin)",
        BACKFILLED.clone(),
    );
    registry.register(
        "hs_room_backfill_batches",
        "Backfilled batches placed, by kind and by where the state at them came from: \
         state_fetched (asked of the server that sent the batch) or state_walked (walked back \
         from what is held, because the server could not answer)",
        BATCHES.clone(),
    );
    registry.register(
        "hs_room_backfill_rejected_events",
        "Backfilled events not placed because authorization refused them at their position, by \
         kind (outcome rejected_auth)",
        REJECTED.clone(),
    );
}

/// The `hs serve` implementation of [`hs_room::backfill::Backfill`]. See the module docs.
pub struct FederationBackfill<B: KvBackend> {
    client: Arc<FederationClient>,
    key_cache: Arc<DynRemoteKeyCache>,
    rooms: Arc<RoomRegistry<B>>,
    identity: HomeserverIdentity,
    /// One lock per room, so that two clients paging the same room to its held edge at once
    /// fetch consecutive batches rather than the same batch twice. Entries are never removed;
    /// there is one per room ever backfilled in this process.
    in_flight: tokio::sync::Mutex<HashMap<OwnedRoomId, Arc<tokio::sync::Mutex<()>>>>,
}

impl<B: KvBackend + 'static> FederationBackfill<B> {
    /// Over the federation mount's own client and key cache, so discovery, TLS trust, request
    /// signing and per-destination backoff are the ones every other outbound call uses.
    #[must_use]
    pub fn new(
        client: Arc<FederationClient>,
        key_cache: Arc<DynRemoteKeyCache>,
        rooms: Arc<RoomRegistry<B>>,
        identity: HomeserverIdentity,
    ) -> Self {
        Self {
            client,
            key_cache,
            rooms,
            identity,
            in_flight: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn room_lock(&self, room_id: &RoomId) -> Arc<tokio::sync::Mutex<()>> {
        self.in_flight
            .lock()
            .await
            .entry(room_id.to_owned())
            .or_default()
            .clone()
    }

    /// Asks each of `servers` (never this one) for a `/backfill` from `from` until one answers,
    /// then verifies, fetches the state for and places what it sent. The outcome, or
    /// [`RoomError::BackfillFailed`] if nobody answered.
    async fn fetch_and_place(
        &self,
        handle: &RoomActorHandle<B>,
        room_id: &RoomId,
        kind: HistoryKind,
        from: &[String],
        servers: &[String],
    ) -> Result<HistoryOutcome, RoomError> {
        let room_version = handle.query(|actor| actor.room_version().clone()).await;
        let own_name = self.identity.server_name.as_str();
        let mut last_error: Option<String> = None;
        for destination in servers.iter().filter(|s| s.as_str() != own_name) {
            let pdus = match self
                .client
                .backfill(destination, room_id.as_str(), from, BATCH)
                .await
            {
                Ok(pdus) => pdus,
                Err(error) => {
                    tracing::warn!(%room_id, destination, kind = kind.label(), %error, "a server could not be asked for the room's history");
                    last_error = Some(format!("{destination}: {error}"));
                    continue;
                }
            };
            let mut events = Vec::with_capacity(pdus.len());
            let mut unverifiable = 0usize;
            for raw in &pdus {
                match verify_pdu(raw, &room_version, &self.key_cache).await {
                    Ok(event) => events.push(event),
                    Err(error) => {
                        unverifiable += 1;
                        tracing::debug!(%room_id, destination, %error, "dropping a backfilled event that does not verify");
                    }
                }
            }
            let fetched = match handle.plan_history(kind, events.clone()).await? {
                Some(plan) => {
                    self.fetch_state(handle, room_id, destination, &room_version, &plan)
                        .await
                }
                None => None,
            };
            let outcome = handle.accept_history(kind, events, fetched).await?;
            count(kind, &outcome);
            tracing::info!(
                %room_id,
                destination,
                kind = kind.label(),
                from = ?from,
                received = pdus.len(),
                unverifiable,
                added = outcome.added,
                rejected = outcome.rejected,
                unchecked = outcome.unchecked,
                state = outcome.state.label(),
                state_events_stored = outcome.state_events_stored,
                gap_closed = outcome.gap_closed,
                "fetched a batch of the room's history"
            );
            return Ok(outcome);
        }
        Err(RoomError::BackfillFailed(last_error.unwrap_or_else(|| {
            "no server to ask: the only candidates were this one".to_owned()
        })))
    }

    /// The state at `plan.oldest` as `destination` answers it, with the events it names that
    /// this server does not hold (see the module docs). `None` when neither `/state_ids` nor
    /// `/state` is answered: the batch is then walked.
    async fn fetch_state(
        &self,
        handle: &RoomActorHandle<B>,
        room_id: &RoomId,
        destination: &str,
        room_version: &RoomVersionId,
        plan: &HistoryPlan,
    ) -> Option<FetchedState> {
        let at = plan.oldest.to_string();
        match self
            .client
            .state_ids(destination, room_id.as_str(), &at)
            .await
        {
            Ok((state_ids, auth_chain_ids)) => {
                let state_ids = parse_ids(&state_ids);
                let mut wanted = state_ids.clone();
                wanted.extend(parse_ids(&auth_chain_ids));
                wanted.extend(plan.missing_auth.iter().cloned());
                let missing = handle
                    .query(move |actor| actor.events_not_held(&wanted))
                    .await;
                let named = state_ids.len() + auth_chain_ids.len();
                if missing.len() > MAX_EVENT_FETCHES
                    || (!missing.is_empty() && missing.len() * 10 >= named)
                {
                    tracing::debug!(%room_id, destination, missing = missing.len(), named, "fetching the whole state at once");
                    if let Some(fetched) = self
                        .fetch_whole_state(room_id, destination, room_version, plan)
                        .await
                    {
                        return Some(fetched);
                    }
                }
                let events = self
                    .fetch_events(room_id, destination, room_version, missing)
                    .await;
                Some(FetchedState {
                    at: plan.oldest.clone(),
                    state_ids,
                    events,
                })
            }
            Err(error) => {
                tracing::warn!(%room_id, destination, %at, %error, "a server could not say the state at a backfilled event by ID; asking for the events");
                self.fetch_whole_state(room_id, destination, room_version, plan)
                    .await
            }
        }
    }

    /// `GET /state` at `plan.oldest`: every state event and its auth chain, verified.
    async fn fetch_whole_state(
        &self,
        room_id: &RoomId,
        destination: &str,
        room_version: &RoomVersionId,
        plan: &HistoryPlan,
    ) -> Option<FetchedState> {
        let at = plan.oldest.to_string();
        let (pdus, auth_chain) = match self
            .client
            .room_state(destination, room_id.as_str(), &at)
            .await
        {
            Ok(answer) => answer,
            Err(error) => {
                tracing::warn!(%room_id, destination, %at, %error, "a server could not say the state at a backfilled event; it is walked instead");
                return None;
            }
        };
        let mut state_ids = Vec::with_capacity(pdus.len());
        let mut events = Vec::with_capacity(pdus.len() + auth_chain.len());
        let mut unverifiable = 0usize;
        for (raw, in_state) in pdus
            .iter()
            .map(|raw| (raw, true))
            .chain(auth_chain.iter().map(|raw| (raw, false)))
        {
            match verify_pdu(raw, room_version, &self.key_cache).await {
                Ok(event) => {
                    if in_state {
                        state_ids.push(event.event_id().to_owned());
                    }
                    events.push(event);
                }
                Err(error) => {
                    unverifiable += 1;
                    tracing::debug!(%room_id, destination, %error, "dropping a state event that does not verify");
                }
            }
        }
        if unverifiable > 0 {
            tracing::warn!(%room_id, destination, unverifiable, "events of a fetched state did not verify; the state lacks them");
        }
        // The batch's own auth events beyond the state, which `/state` does not carry.
        let have: HashSet<OwnedEventId> = events.iter().map(|e| e.event_id().to_owned()).collect();
        let extra: Vec<OwnedEventId> = plan
            .missing_auth
            .iter()
            .filter(|id| !have.contains(*id))
            .take(MAX_EVENT_FETCHES)
            .cloned()
            .collect();
        events.extend(
            self.fetch_events(room_id, destination, room_version, extra)
                .await,
        );
        Some(FetchedState {
            at: plan.oldest.clone(),
            state_ids,
            events,
        })
    }

    /// `GET /event/{eventId}` for each of `ids` (at most [`MAX_EVENT_FETCHES`], a few at a
    /// time), verified, and checked to be the event asked for. What fails is left out.
    async fn fetch_events(
        &self,
        room_id: &RoomId,
        destination: &str,
        room_version: &RoomVersionId,
        ids: Vec<OwnedEventId>,
    ) -> Vec<Event> {
        let permits = Arc::new(tokio::sync::Semaphore::new(EVENT_FETCH_CONCURRENCY));
        let mut tasks = tokio::task::JoinSet::new();
        for id in ids.into_iter().take(MAX_EVENT_FETCHES) {
            let client = self.client.clone();
            let key_cache = self.key_cache.clone();
            let permits = permits.clone();
            let destination = destination.to_owned();
            let room_version = room_version.clone();
            tasks.spawn(async move {
                let _permit = permits.acquire_owned().await.ok()?;
                let raw: Value = match client.event(&destination, id.as_str()).await {
                    Ok(raw) => raw,
                    Err(error) => {
                        tracing::debug!(destination, event_id = %id, %error, "an event of a fetched state could not be fetched");
                        return None;
                    }
                };
                match verify_pdu(&raw, &room_version, &key_cache).await {
                    Ok(event) if event.event_id() == id => Some(event),
                    Ok(event) => {
                        tracing::warn!(destination, asked = %id, got = %event.event_id(), "a server answered /event with another event");
                        None
                    }
                    Err(error) => {
                        tracing::debug!(destination, event_id = %id, %error, "an event of a fetched state does not verify");
                        None
                    }
                }
            });
        }
        let mut events = Vec::new();
        let mut failed = 0usize;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Some(event)) => events.push(event),
                _ => failed += 1,
            }
        }
        if failed > 0 {
            tracing::warn!(%room_id, destination, failed, fetched = events.len(), "events of a fetched state could not be had; the state lacks them");
        }
        events
    }
}

/// The well-formed event IDs of `ids`; a malformed one is dropped.
fn parse_ids(ids: &[String]) -> Vec<OwnedEventId> {
    ids.iter()
        .filter_map(|id| OwnedEventId::try_from(id.as_str()).ok())
        .collect()
}

#[async_trait]
impl<B: KvBackend + 'static> hs_room::backfill::Backfill for FederationBackfill<B> {
    async fn backfill(&self, room_id: &RoomId) -> Result<usize, RoomError> {
        let handle = self.rooms.get_or_load(room_id).await?;
        let lock = self.room_lock(room_id).await;
        let _guard = lock.lock().await;

        // Read after taking the lock: a batch another request just placed moves the anchor.
        let Some(anchor) = handle.query(|actor| actor.backfill_anchor()).await else {
            return Ok(0);
        };
        if anchor.servers.is_empty() {
            tracing::debug!(%room_id, "the room's history continues before what is held, but nobody else is in it to ask");
            return Ok(0);
        }
        let outcome = self
            .fetch_and_place(
                &handle,
                room_id,
                HistoryKind::BeforeOldest,
                &[anchor.event_id.to_string()],
                &anchor.servers,
            )
            .await?;
        Ok(outcome.added)
    }

    async fn fill_gap(&self, room_id: &RoomId, top: i64) -> Result<usize, RoomError> {
        let handle = self.rooms.get_or_load(room_id).await?;
        let lock = self.room_lock(room_id).await;
        let _guard = lock.lock().await;

        // Read after taking the lock: a batch another request just placed moves the anchor, or
        // closes the gap.
        let Some(anchor) = handle.query(move |actor| actor.gap_anchor(top)).await else {
            return Ok(0);
        };
        if anchor.servers.is_empty() {
            tracing::debug!(%room_id, top, "a timeline gap is open, but nobody else is in the room to ask");
            return Ok(0);
        }
        let from: Vec<String> = anchor.from.iter().map(ToString::to_string).collect();
        let outcome = self
            .fetch_and_place(
                &handle,
                room_id,
                HistoryKind::Gap { top },
                &from,
                &anchor.servers,
            )
            .await?;
        Ok(outcome.added)
    }
}
