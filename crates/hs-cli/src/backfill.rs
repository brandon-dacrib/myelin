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
//! `RoomActor::accept_backfilled_events`, which places the batch in the timeline below
//! everything held.
//!
//! The same endpoint the inbound side already used for a different purpose:
//! `hs_federation::backfill::resolve_missing_ancestors` fetches the missing *ancestors* of an
//! event that arrived over `/send`, so that event can be authorized. This is the other trigger
//! for the same fetch -- a client reading backwards -- and it goes through the room actor's
//! history path rather than the ancestor-resolution one, because the events are not there to
//! authorize something newer; they are the history itself.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use hs_federation::client::FederationClient;
use hs_federation::inbound::verify_pdu;
use hs_federation::keys::DynRemoteKeyCache;
use hs_kv::KvBackend;
use hs_room::RoomError;
use hs_room::identity::HomeserverIdentity;
use hs_room::registry::RoomRegistry;
use ruma::{OwnedRoomId, RoomId};

/// How many events one `/backfill` request asks for. The most this server's own side of that
/// endpoint will answer (`hs_federation::transport::read_routes`' clamp), and Synapse's number
/// too, which is the one Complement's `TestMessagesOverFederation` is written around: a
/// `/messages` page of more than this comes back short and with an `end`, and the next page
/// fetches the next batch.
pub const BATCH: usize = 100;

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
        let room_version = handle.query(|actor| actor.room_version().clone()).await;
        let own_name = self.identity.server_name.as_str();

        let mut last_error: Option<String> = None;
        for destination in anchor.servers.iter().filter(|s| s.as_str() != own_name) {
            let pdus = match self
                .client
                .backfill(
                    destination,
                    room_id.as_str(),
                    &[anchor.event_id.to_string()],
                    BATCH,
                )
                .await
            {
                Ok(pdus) => pdus,
                Err(error) => {
                    tracing::warn!(%room_id, destination, %error, "a server could not be asked for the room's earlier history");
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
            let added = handle.accept_backfilled_events(events).await?;
            tracing::info!(
                %room_id,
                destination,
                anchor = %anchor.event_id,
                received = pdus.len(),
                unverifiable,
                added,
                "fetched the room's earlier history"
            );
            return Ok(added);
        }
        Err(RoomError::BackfillFailed(last_error.unwrap_or_else(|| {
            "no server to ask: the only candidates were this one".to_owned()
        })))
    }
}
