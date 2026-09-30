//! `hs_room::hierarchy::RemoteHierarchy` over the federation client: how the client-server
//! `GET /rooms/{roomId}/hierarchy` learns about a room of a space that this server does not
//! hold.
//!
//! The same meeting point as [`crate::remote_join`] and [`crate::backfill`]: `hs-room` walks the
//! space and knows which child it cannot vouch for and which servers its `m.space.child` link
//! names; `hs-federation` speaks to other servers; this module joins them. One call is one
//! `GET /_matrix/federation/v1/hierarchy/{roomId}` against one server, and the walk tries the
//! next server in `via` when it fails. The answer is a description, not room content: it is
//! shown to the requesting user only after `hs_room::hierarchy` has judged, from the summary
//! and the user's own memberships, that they may see the room.

use std::sync::Arc;

use async_trait::async_trait;
use hs_federation::client::FederationClient;
use hs_room::RoomError;
use hs_room::hierarchy::{RemoteHierarchy, RemoteHierarchyPage};
use ruma::RoomId;

/// The `hs serve` implementation of [`RemoteHierarchy`]. See the module docs.
pub struct FederationHierarchy {
    client: Arc<FederationClient>,
}

impl FederationHierarchy {
    /// Over the federation mount's own client, so discovery, TLS trust, request signing and
    /// per-destination backoff are the ones every other outbound call uses.
    #[must_use]
    pub fn new(client: Arc<FederationClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RemoteHierarchy for FederationHierarchy {
    async fn fetch(
        &self,
        destination: &str,
        room_id: &RoomId,
        suggested_only: bool,
    ) -> Result<RemoteHierarchyPage, RoomError> {
        let body = self
            .client
            .room_hierarchy(destination, room_id.as_str(), suggested_only)
            .await
            .map_err(|error| RoomError::Internal(format!("{destination}: {error}")))?;
        RemoteHierarchyPage::from_json(&body)
    }
}
