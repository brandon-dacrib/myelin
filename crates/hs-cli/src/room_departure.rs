//! How a self-service deactivation (`POST /account/deactivate`) leaves the account's rooms:
//! `hs-auth`'s [`hs_auth::state::RoomDeparture`] hook, answered by the same room-layer adapter
//! an administrator's `users.deactivate` with `erase: true` uses
//! ([`hs_room::admin_users::RoomRegistryUserActivity::leave_all_rooms`]), so the two leave rooms
//! the same way: through the room when a user of this server is joined to it, through another
//! server otherwise, and by rejecting the invite or knock alone when none will take the leave.
//!
//! Like [`crate::profile_refresh`], the hook lives inside the [`AuthState`] the room registry
//! holds, so it holds the registry weakly and builds the adapter per call.

use std::sync::{Arc, Weak};

use hs_admin::user_moderation::UserActivitySource;
use hs_auth::state::RoomDepartureReport;
use hs_kv::KvBackend;
use hs_room::admin_users::RoomRegistryUserActivity;
use hs_room::registry::RoomRegistry;
use hs_room::remote_join::RemoteJoin;
use ruma::UserId;

/// The room layer's departure, as `hs-auth` asks for it.
pub struct RoomDepartureSource<B: KvBackend + 'static> {
    rooms: Weak<RoomRegistry<B>>,
    remote_join: Option<Arc<dyn RemoteJoin>>,
}

impl<B: KvBackend + 'static> RoomDepartureSource<B> {
    /// Over the server's room registry, held weakly (see the module docs), and the federation
    /// hook a leave for a room nobody of this server is joined to goes through (`None` without
    /// federation: such an invite or knock is rejected here alone).
    #[must_use]
    pub fn new(rooms: &Arc<RoomRegistry<B>>, remote_join: Option<Arc<dyn RemoteJoin>>) -> Self {
        Self {
            rooms: Arc::downgrade(rooms),
            remote_join,
        }
    }
}

#[async_trait::async_trait]
impl<B: KvBackend + 'static> hs_auth::state::RoomDeparture for RoomDepartureSource<B> {
    async fn leave_all_rooms(&self, user_id: &UserId) -> Result<RoomDepartureReport, String> {
        let Some(rooms) = self.rooms.upgrade() else {
            return Err("the room registry is gone (shutting down)".to_owned());
        };
        let activity =
            RoomRegistryUserActivity::new(rooms).with_remote_join(self.remote_join.clone());
        let report = activity
            .leave_all_rooms(user_id.as_str())
            .await
            .map_err(|e| e.to_string())?;
        Ok(RoomDepartureReport {
            rooms_left: report.rooms_left,
            rooms_failed: report
                .rooms_failed
                .into_iter()
                .map(|f| (f.room_id, f.reason))
                .collect(),
        })
    }
}
