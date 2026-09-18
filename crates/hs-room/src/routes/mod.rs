//! The client-server HTTP endpoints for rooms, as a router fragment. Mount under
//! `/_matrix/client/v3` (and the historical `r0` alias) alongside `hs-auth`'s and every other
//! track's routers, the way `hs-media` and `hs-auth` do (`hs_auth::routes::router`,
//! `hs_media::router::authenticated_router`): [`router`] returns spec-relative paths, not
//! prefixed ones.

pub mod aliases;
pub mod create_room;
pub mod membership;
pub mod query;
pub mod redact;
pub mod relations;
pub mod render;
pub mod send_state;

use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_kv::KvBackend;

use crate::state::RoomState;

fn matrix_client(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixClient, AuthKind::Matrix).with_operation_id(operation_id)
}

/// The room endpoints router fragment and its `routes.json` manifest.
pub fn router<B: KvBackend + 'static>() -> (axum::Router<RoomState<B>>, RouteManifest) {
    Builder::new()
        .post(
            "/createRoom",
            create_room::post_create_room::<B>,
            matrix_client("createRoom"),
        )
        .put(
            "/rooms/{roomId}/send/{eventType}/{txnId}",
            send_state::put_send::<B>,
            matrix_client("sendMessage"),
        )
        .put(
            "/rooms/{roomId}/state/{eventType}/{stateKey}",
            send_state::put_state::<B>,
            matrix_client("setStateEventWithKey"),
        )
        .put(
            "/rooms/{roomId}/state/{eventType}",
            send_state::put_state_no_key::<B>,
            matrix_client("setStateEvent"),
        )
        .get(
            "/rooms/{roomId}/state/{eventType}/{stateKey}",
            query::get_state_with_key::<B>,
            matrix_client("getStateEventWithKey"),
        )
        .get(
            "/rooms/{roomId}/state/{eventType}",
            query::get_state_no_key::<B>,
            matrix_client("getStateEvent"),
        )
        .get(
            "/rooms/{roomId}/state",
            query::get_state::<B>,
            matrix_client("getRoomState"),
        )
        .get(
            "/rooms/{roomId}/event/{eventId}",
            query::get_event::<B>,
            matrix_client("getOneRoomEvent"),
        )
        .get(
            "/rooms/{roomId}/context/{eventId}",
            query::get_context::<B>,
            matrix_client("getEventContext"),
        )
        .get(
            "/rooms/{roomId}/members",
            query::get_members::<B>,
            matrix_client("getMembersByRoom"),
        )
        .get(
            "/rooms/{roomId}/joined_members",
            query::get_joined_members::<B>,
            matrix_client("getJoinedMembersByRoom"),
        )
        .get(
            "/rooms/{roomId}/messages",
            query::get_messages::<B>,
            matrix_client("getRoomEvents"),
        )
        .post(
            "/rooms/{roomId}/join",
            membership::post_join::<B>,
            matrix_client("joinRoomById"),
        )
        .post(
            "/join/{roomIdOrAlias}",
            membership::post_join_by_id_or_alias::<B>,
            matrix_client("joinRoom"),
        )
        .post(
            "/rooms/{roomId}/leave",
            membership::post_leave::<B>,
            matrix_client("leaveRoom"),
        )
        .post(
            "/rooms/{roomId}/forget",
            membership::post_forget::<B>,
            matrix_client("forgetRoom"),
        )
        .post(
            "/rooms/{roomId}/invite",
            membership::post_invite::<B>,
            matrix_client("inviteUser"),
        )
        .post(
            "/rooms/{roomId}/kick",
            membership::post_kick::<B>,
            matrix_client("kick"),
        )
        .post(
            "/rooms/{roomId}/ban",
            membership::post_ban::<B>,
            matrix_client("ban"),
        )
        .post(
            "/rooms/{roomId}/unban",
            membership::post_unban::<B>,
            matrix_client("unban"),
        )
        .post(
            "/rooms/{roomId}/knock",
            membership::post_knock::<B>,
            matrix_client("knockRoom"),
        )
        .post(
            "/knock/{roomIdOrAlias}",
            membership::post_knock_by_id_or_alias::<B>,
            matrix_client("knock"),
        )
        .put(
            "/rooms/{roomId}/redact/{eventId}/{txnId}",
            redact::put_redact::<B>,
            matrix_client("redactEvent"),
        )
        .get(
            "/rooms/{roomId}/aliases",
            aliases::get_room_aliases::<B>,
            matrix_client("getRoomAliases"),
        )
        .put(
            "/directory/room/{roomAlias}",
            aliases::put_alias::<B>,
            matrix_client("setRoomAlias"),
        )
        .get(
            "/directory/room/{roomAlias}",
            aliases::get_alias::<B>,
            matrix_client("getRoomIdByAlias"),
        )
        .delete(
            "/directory/room/{roomAlias}",
            aliases::delete_alias::<B>,
            matrix_client("removeRoomAlias"),
        )
        .get(
            "/rooms/{roomId}/relations/{eventId}",
            relations::get_relations::<B>,
            matrix_client("getRelatingEvents"),
        )
        .get(
            "/rooms/{roomId}/relations/{eventId}/{relType}",
            relations::get_relations_by_rel_type::<B>,
            matrix_client("getRelatingEventsWithRelType"),
        )
        .get(
            "/rooms/{roomId}/relations/{eventId}/{relType}/{eventType}",
            relations::get_relations_by_rel_type_and_event_type::<B>,
            matrix_client("getRelatingEventsWithRelTypeAndEventType"),
        )
        .build()
}
