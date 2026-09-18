//! The client-server HTTP endpoints this crate owns, as a router fragment. Mount under
//! `/_matrix/client/v3` (and the historical `r0` alias), the same way `hs-room` and `hs-media` do
//! -- [`router`] returns spec-relative paths, not prefixed ones. See `crate::state::UserState` for
//! the shared state every handler here takes, and `docs/status/05-sync.md` for the exact addition
//! `hs-cli` needs to actually mount this.

pub mod account_data;
pub mod filter;
pub mod rooms;
pub mod sync;

use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_kv::KvBackend;

use crate::room_source::RoomSource;
use crate::state::UserState;

fn matrix_client(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixClient, AuthKind::Matrix).with_operation_id(operation_id)
}

/// This crate's endpoints and their `routes.json` manifest: `/sync`, `/joined_rooms`,
/// `/publicRooms`, account data (global and room-scoped) and filters.
pub fn router<B: KvBackend + 'static, R: RoomSource<B> + 'static>()
-> (axum::Router<UserState<B, R>>, RouteManifest) {
    Builder::new()
        .get("/sync", sync::get_sync::<B, R>, matrix_client("sync"))
        .get(
            "/joined_rooms",
            rooms::get_joined_rooms::<B, R>,
            matrix_client("getJoinedRooms"),
        )
        .get(
            "/publicRooms",
            rooms::get_public_rooms::<B, R>,
            matrix_client("publicRooms"),
        )
        .post(
            "/publicRooms",
            rooms::post_public_rooms::<B, R>,
            matrix_client("queryPublicRooms"),
        )
        .get(
            "/user/{userId}/account_data/{type}",
            account_data::get_global::<B, R>,
            matrix_client("getAccountData"),
        )
        .put(
            "/user/{userId}/account_data/{type}",
            account_data::put_global::<B, R>,
            matrix_client("setAccountData"),
        )
        .get(
            "/user/{userId}/rooms/{roomId}/account_data/{type}",
            account_data::get_room::<B, R>,
            matrix_client("getRoomAccountData"),
        )
        .put(
            "/user/{userId}/rooms/{roomId}/account_data/{type}",
            account_data::put_room::<B, R>,
            matrix_client("setRoomAccountData"),
        )
        .post(
            "/user/{userId}/filter",
            filter::post_filter::<B, R>,
            matrix_client("defineFilter"),
        )
        .get(
            "/user/{userId}/filter/{filterId}",
            filter::get_filter::<B, R>,
            matrix_client("getFilter"),
        )
        .build()
}
