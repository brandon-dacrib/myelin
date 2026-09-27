//! The seam that lets `POST /createRoom` be shard-gated even though the room's id does not
//! exist until the handler mints it.
//!
//! Every `/rooms/{roomId}/...` request carries its room id in the path, so the routing gate
//! (`hs-cli`'s `RoomShardGate`) can hash it to a shard and forward the request to that shard's
//! owner before any local actor is built. `/createRoom` cannot be gated that way: the id is
//! chosen inside the handler. The fix is to move the choice *in front of* the gate: the gate
//! mints the id itself (`!random:server`, the same shape `ruma::RoomId::new_v1` produces), hashes
//! it, and either handles the request locally (it owns the shard) or forwards it to the owner,
//! carrying the id in the [`PREASSIGNED_ROOM_ID_HEADER`] mesh header. On whichever replica ends
//! up running the handler, the gate turns the header into a [`PreassignedRoomId`] request
//! extension, and the handler creates the room under that id instead of minting another. That
//! is what makes "the room's first actor is built on its owner" true.
//!
//! Two properties keep this safe:
//!
//! - **A client can never choose a room id.** The header is honoured only on a request that
//!   arrived over the mesh (marked with the [`ViaMesh`] extension by the owner's mesh handler
//!   before it re-enters the router); on a request from a client socket the gate strips any
//!   value the client sent and mints its own. Extensions cannot be set from outside the process,
//!   so a handler that reads [`PreassignedRoomId`] can trust it unconditionally.
//! - **The owner re-checks.** A forwarded id is accepted only if the receiving replica owns its
//!   shard at that moment; otherwise the request is refused and the sender's forwarder retries
//!   against the current owner. A pre-minted id therefore never lets a stale ownership view on
//!   the sender build an actor on a non-owner.
//!
//! These types live in `hs-cluster` rather than `hs-cli` so `hs-room` (which already depends on
//! this crate for fencing) can read the extension without a dependency on the binary crate. The
//! change `hs-room`'s handler needs is one line: `room_id: preassigned.map(|p| p.0)`; see
//! `docs/rfcs/0019-create-room-shard-gate.md`.
//!
//! Rooms whose id is derived from the create event's hash (room version 12's `!hash` ids) cannot
//! be pre-minted at all; for those the handler must instead retry creation until the derived id
//! hashes to a shard this replica owns. That is the handler's side of the RFC, not this seam's.

/// The mesh-internal header a forwarded `/createRoom` carries its pre-minted room id in. Never
/// honoured from a client: the routing gate strips it from every request that did not arrive
/// over the mesh.
pub const PREASSIGNED_ROOM_ID_HEADER: &str = "x-hs-preassigned-room-id";

/// A request extension marking a request that re-entered this replica's router from its mesh
/// handler (a forward from a peer), as opposed to one that arrived on a client socket. The
/// routing gate uses it to decide whether [`PREASSIGNED_ROOM_ID_HEADER`] is trustworthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViaMesh;

/// A request extension carrying the room id the routing gate chose for this `/createRoom`.
/// Present only on a request the gate has confirmed this replica should handle (it owns the
/// id's shard). A handler that finds it must create the room under exactly this id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreassignedRoomId(pub String);

impl PreassignedRoomId {
    /// The room id, `!localpart:server`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PreassignedRoomId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_name_is_lowercase_and_hs_prefixed() {
        // `http::HeaderName::from_static` requires lowercase; the other mesh headers follow the
        // same `x-hs-` convention (`crate::mesh::envelope::headers`).
        assert_eq!(
            PREASSIGNED_ROOM_ID_HEADER,
            PREASSIGNED_ROOM_ID_HEADER.to_ascii_lowercase()
        );
        assert!(PREASSIGNED_ROOM_ID_HEADER.starts_with("x-hs-"));
        assert!(http::HeaderName::from_bytes(PREASSIGNED_ROOM_ID_HEADER.as_bytes()).is_ok());
    }

    #[test]
    fn preassigned_id_displays_verbatim() {
        let id = PreassignedRoomId("!abc:example.org".into());
        assert_eq!(id.to_string(), "!abc:example.org");
        assert_eq!(id.as_str(), "!abc:example.org");
    }
}
