//! How an administrator's profile change reaches the user's rooms: `hs-auth`'s
//! [`hs_auth::state::ProfileRefresh`] hook, answered by `hs-room`'s own fan-out.
//!
//! `hs-admin`'s `users.update` writes a display name or avatar through
//! `hs_auth::admin_directory::AuthStoreUserDirectory::update_profile`, which cannot reach rooms
//! (`hs-auth` is below `hs-room`). This is the piece `hs serve` installs so that the change is
//! carried the way the user's own `PUT /profile/{userId}/displayname` is carried
//! (`hs_room::routes::profile::spawn_refresh`): the user's `m.room.member` event is re-sent with
//! the new values in every room they are joined to, which is what other clients' `/sync` and,
//! through the federation sender, other servers see.
//!
//! The hook lives inside the [`AuthState`] it serves, and the room registry holds that state,
//! so it holds the registry weakly and is handed the auth state per call: a strong reference
//! either way would be a cycle that keeps the stores alive after shutdown
//! (`tests/in_process_restart.rs` names what leaks).

use std::sync::{Arc, Weak};

use hs_auth::state::AuthState;
use hs_kv::KvBackend;
use hs_room::registry::RoomRegistry;
use ruma::UserId;

/// The room layer's profile fan-out, as `hs-auth` asks for it.
pub struct RoomProfileRefresh<B: KvBackend + 'static> {
    rooms: Weak<RoomRegistry<B>>,
}

impl<B: KvBackend + 'static> RoomProfileRefresh<B> {
    /// Over the server's room registry, held weakly (see the module docs).
    #[must_use]
    pub fn new(rooms: &Arc<RoomRegistry<B>>) -> Self {
        Self {
            rooms: Arc::downgrade(rooms),
        }
    }
}

impl<B: KvBackend + 'static> hs_auth::state::ProfileRefresh for RoomProfileRefresh<B> {
    fn profile_changed(&self, auth: &AuthState, user_id: &UserId) {
        let Some(rooms) = self.rooms.upgrade() else {
            tracing::debug!(user = %user_id, "the room registry is gone (shutting down); a profile change is not carried into rooms");
            return;
        };
        tracing::debug!(user = %user_id, "re-stamping a user's membership after a profile change");
        hs_room::routes::profile::spawn_refresh(rooms, auth.clone(), user_id.to_string());
    }
}
