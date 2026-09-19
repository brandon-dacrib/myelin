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

/// This crate's endpoints and their `routes.json` manifest: `/sync`, `/joined_rooms`, account
/// data (global and room-scoped) and filters.
///
/// `/publicRooms` (`crate::routes::rooms::get_public_rooms`/`post_public_rooms`) is deliberately
/// **not** mounted here as of this session: `docs/workstreams/04-room-and-events.md` lists
/// "aliases and directory" under track 04's ownership, and `hs-room` has since landed its own
/// `GET`/`POST /publicRooms` (`crates/hs-room/src/routes/directory.rs`). Mounting both panics at
/// router-build time (`hs-http`'s `Builder` rejects an overlapping method+path registration) --
/// discovered this session as a hard boot failure of the real `hs` binary once both routers were
/// merged in `hs-cli`. This crate's own implementation
/// (`crate::routes::rooms::{get_public_rooms, post_public_rooms}`, backed by
/// `crate::store::UserStore::list_public_rooms`/`crate::hub`'s directory-entry population) is left
/// in place, unmounted, rather than deleted, in case track 04's version turns out to need
/// something this one already has -- see `docs/status/05-sync.md`.
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
