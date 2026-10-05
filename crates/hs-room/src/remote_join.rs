//! [`RemoteJoin`]: the seam through which a join of a room this server does not hold reaches
//! federation -- and the rest of membership that has to go through another server: leaving or
//! knocking on a room this server is not in, and inviting a user of another server, whose
//! server co-signs the invite before it goes into the room.
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

    /// Leaves `room_id` as `user_id` through one of `via` (`make_leave`/`send_leave`) -- a
    /// room this server is not in, so the leave cannot be made here: rejecting an invite from
    /// another server, or withdrawing a knock -- and records the accepted leave here so the
    /// user's `/sync` moves the room to `leave`. `content` is the rest of the leave's content
    /// (`reason`).
    ///
    /// The default refuses: an implementation that only joins cannot leave this way.
    ///
    /// # Errors
    /// As [`RemoteJoin::join`].
    async fn leave(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        via: &[String],
        content: Value,
    ) -> Result<(), RoomError> {
        let _ = (user_id, via, content);
        Err(RoomError::RemoteJoinFailed(format!(
            "cannot leave {room_id} through another server"
        )))
    }

    /// Knocks on `room_id` as `user_id` through one of `via` (`make_knock`/`send_knock`), and
    /// records the accepted knock here, with the room's stripped state the resident answered
    /// with, so the user's `/sync` shows it under `knock`. `content` is the rest of the knock's
    /// content (`reason`, the user's profile).
    ///
    /// The default refuses.
    ///
    /// # Errors
    /// As [`RemoteJoin::join`].
    async fn knock(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        via: &[String],
        content: Value,
    ) -> Result<OwnedRoomId, RoomError> {
        let _ = (user_id, via, content);
        Err(RoomError::RemoteJoinFailed(format!(
            "cannot knock on {room_id} through another server"
        )))
    }

    /// Sends `event` -- an invite built and signed here, not yet in the room
    /// (`RoomActor::build_membership_event`), for a user of another server -- to that server
    /// with the room's stripped state (`PUT /invite`), and returns the event as it came back,
    /// co-signed by the invitee's server and verified. The caller puts that into the room.
    ///
    /// The default refuses.
    ///
    /// # Errors
    /// [`RoomError::Forbidden`] if the invitee's server refused the invite,
    /// [`RoomError::RemoteJoinFailed`] if it could not be asked or answered with something that
    /// is not the invite.
    async fn invite(
        &self,
        room_version: &ruma::RoomVersionId,
        event: &hs_model::Event,
        invite_room_state: Vec<Value>,
    ) -> Result<hs_model::Event, RoomError> {
        let _ = (room_version, invite_room_state);
        Err(RoomError::RemoteJoinFailed(format!(
            "cannot send the invite {} to another server",
            event.event_id()
        )))
    }

    /// Hands a bound third-party invitation for `room_id`, a room this server is not in, to
    /// `destination` -- the server of whoever made the invitation, which is in the room --
    /// (`PUT /_matrix/federation/v1/exchange_third_party_invite/{roomId}`), which turns it into
    /// the invite and sends it here. `event` is the spec's body: the `m.room.member` invite's
    /// `type`, `room_id`, `sender`, `state_key` and `content` (`membership` and
    /// `third_party_invite.signed`).
    ///
    /// The default refuses.
    ///
    /// # Errors
    /// [`RoomError::Forbidden`] if `destination` refused the invitation,
    /// [`RoomError::RemoteJoinFailed`] if it could not be asked.
    async fn exchange_third_party_invite(
        &self,
        destination: &str,
        room_id: &RoomId,
        event: Value,
    ) -> Result<(), RoomError> {
        let _ = (destination, event);
        Err(RoomError::RemoteJoinFailed(format!(
            "cannot hand a third-party invitation for {room_id} to another server"
        )))
    }

    /// `server`'s public room directory (`GET`/`POST /_matrix/federation/v1/publicRooms`), the
    /// page as that server answered it (`chunk`, `next_batch`, `prev_batch`,
    /// `total_room_count_estimate`): what `GET /publicRooms?server=` passes through. `limit`
    /// and `since` page it; `search` narrows it (a `POST` with `filter.generic_search_term`).
    ///
    /// # Errors
    /// [`RoomError::RemoteJoinFailed`] if the server could not be asked or answered with
    /// something other than a room list (`502` to the client).
    async fn public_rooms(
        &self,
        server: &str,
        limit: Option<usize>,
        since: Option<&str>,
        search: Option<&str>,
    ) -> Result<Value, RoomError> {
        let _ = (limit, since, search);
        Err(RoomError::RemoteJoinFailed(format!(
            "cannot fetch the public room list of {server}"
        )))
    }
}
