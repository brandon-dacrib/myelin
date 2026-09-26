//! [`RemoteJoin`]: the seam through which a join of a room this server does not hold reaches
//! federation.
//!
//! `POST /join/{roomIdOrAlias}` and `POST /rooms/{roomId}/join` (`crate::routes::membership`)
//! are served by this crate, which knows nothing about federation: `hs-federation` and `hs-room`
//! are independent crates, and `hs-cli` is where they meet. So when a client asks to join a room
//! the registry has never heard of, the route hands the request to whatever implements this
//! trait -- in `hs serve`, an adapter over `hs_federation::outbound_join::join_room` and
//! `crate::registry::RoomRegistry::bootstrap_from_remote_join` -- and, when nothing does (a
//! server with federation disabled, this crate's own tests), the room is simply not found, as it
//! always was.
//!
//! The same shape as `crate::registry::GlobalTokenResolver` and `crate::fencing`: a hook that
//! `hs-cli` installs so that this crate's routes gain a capability without this crate gaining a
//! dependency.

use async_trait::async_trait;
use ruma::{OwnedRoomId, RoomAliasId, RoomId, UserId};
use serde_json::Value;

use crate::error::RoomError;

/// Joins rooms hosted elsewhere, and resolves aliases hosted elsewhere, on behalf of this
/// server's own users. See the module docs.
#[async_trait]
pub trait RemoteJoin: Send + Sync {
    /// Joins `room_id` as `user_id`, asking each server in `via` in turn to sponsor the join
    /// until one does, and makes the room resident locally so the user's next `/sync` carries
    /// it. `content` is what the client asked to put into its own `m.room.member` event beyond
    /// `membership` itself (`reason`, the user's profile); the implementation merges it into the
    /// join template the sponsoring server hands back.
    ///
    /// # Errors
    /// [`RoomError::Forbidden`] if the room refused the join, [`RoomError::RoomNotFound`] if no
    /// server in `via` knows the room, [`RoomError::RemoteJoinFailed`] if none of them could be
    /// reached or answered sensibly, and whatever making the room resident can fail with.
    async fn join(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        via: &[String],
        content: Value,
    ) -> Result<OwnedRoomId, RoomError>;

    /// Resolves `alias`, whose server is not this one, through that server's directory
    /// (`GET /_matrix/federation/v1/query/directory`). Returns the room ID and the servers the
    /// directory named as being in the room, which are the natural `via` for the join that
    /// usually follows.
    ///
    /// # Errors
    /// [`RoomError::RoomNotFound`] if the alias's server does not know it,
    /// [`RoomError::RemoteJoinFailed`] if that server could not be asked.
    async fn resolve_alias(
        &self,
        alias: &RoomAliasId,
    ) -> Result<(OwnedRoomId, Vec<String>), RoomError>;
}
