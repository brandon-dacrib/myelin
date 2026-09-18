//! The client-server HTTP endpoints this crate owns, as router fragments. Mount [`router`] under
//! `/_matrix/client/v3` (and the historical `r0` alias), the way `hs-room` and `hs-auth` do; mount
//! [`unstable_router`] under `/_matrix/client/unstable` directly (MSC3983/MSC3984 have never had a
//! versioned path). See `docs/status/08-e2ee.md`'s "Interfaces provided" for the exact
//! `hs-cli/src/serve.rs` wiring this expects, mirroring `docs/status/07-auth-and-identity.md`'s
//! equivalent section for `hs-auth`.

pub mod appservice_proxy;
pub mod cross_signing;
pub mod keys_changes;
pub mod keys_claim;
pub mod keys_query;
pub mod keys_upload;
pub mod room_keys;
pub mod to_device;

use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_kv::KvBackend;

use crate::state::E2eState;

fn matrix_client(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixClient, AuthKind::Matrix).with_operation_id(operation_id)
}

fn matrix_appservice(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixAppservice, AuthKind::Appservice).with_operation_id(operation_id)
}

/// The spec-relative (no version prefix) client-server router fragment: device keys, one-time and
/// fallback keys, cross-signing, key backups, to-device messaging.
pub fn router<B: KvBackend + 'static>() -> (axum::Router<E2eState<B>>, RouteManifest) {
    Builder::new()
        .post(
            "/keys/upload",
            keys_upload::post_keys_upload::<B>,
            matrix_client("uploadKeys"),
        )
        .post(
            "/keys/query",
            keys_query::post_keys_query::<B>,
            matrix_client("queryKeys"),
        )
        .post(
            "/keys/claim",
            keys_claim::post_keys_claim::<B>,
            matrix_client("claimKeys"),
        )
        .get(
            "/keys/changes",
            keys_changes::get_keys_changes::<B>,
            matrix_client("getKeysChanges"),
        )
        .post(
            "/keys/device_signing/upload",
            cross_signing::post_device_signing_upload::<B>,
            matrix_client("uploadCrossSigningKeys"),
        )
        .post(
            "/keys/signatures/upload",
            cross_signing::post_signatures_upload::<B>,
            matrix_client("uploadCrossSigningSignatures"),
        )
        .put(
            "/sendToDevice/{eventType}/{txnId}",
            to_device::put_send_to_device::<B>,
            matrix_client("sendToDevice"),
        )
        .get(
            "/room_keys/version",
            room_keys::get_version_latest::<B>,
            matrix_client("getRoomKeysVersionCurrent"),
        )
        .get(
            "/room_keys/version/{version}",
            room_keys::get_version::<B>,
            matrix_client("getRoomKeysVersion"),
        )
        .post(
            "/room_keys/version",
            room_keys::post_version::<B>,
            matrix_client("postRoomKeysVersion"),
        )
        .put(
            "/room_keys/version/{version}",
            room_keys::put_version::<B>,
            matrix_client("putRoomKeysVersion"),
        )
        .delete(
            "/room_keys/version/{version}",
            room_keys::delete_version::<B>,
            matrix_client("deleteRoomKeysVersion"),
        )
        .get(
            "/room_keys/keys",
            room_keys::get_keys_all::<B>,
            matrix_client("getRoomKeys"),
        )
        .get(
            "/room_keys/keys/{roomId}",
            room_keys::get_keys_room::<B>,
            matrix_client("getRoomKeysByRoom"),
        )
        .get(
            "/room_keys/keys/{roomId}/{sessionId}",
            room_keys::get_keys_session::<B>,
            matrix_client("getRoomKeysBySession"),
        )
        .put(
            "/room_keys/keys",
            room_keys::put_keys_all::<B>,
            matrix_client("putRoomKeys"),
        )
        .put(
            "/room_keys/keys/{roomId}",
            room_keys::put_keys_room::<B>,
            matrix_client("putRoomKeysByRoom"),
        )
        .put(
            "/room_keys/keys/{roomId}/{sessionId}",
            room_keys::put_keys_session::<B>,
            matrix_client("putRoomKeysBySession"),
        )
        .delete(
            "/room_keys/keys",
            room_keys::delete_keys_all::<B>,
            matrix_client("deleteRoomKeys"),
        )
        .delete(
            "/room_keys/keys/{roomId}",
            room_keys::delete_keys_room::<B>,
            matrix_client("deleteRoomKeysByRoom"),
        )
        .delete(
            "/room_keys/keys/{roomId}/{sessionId}",
            room_keys::delete_keys_session::<B>,
            matrix_client("deleteRoomKeysBySession"),
        )
        .build()
}

/// MSC3983/MSC3984's appservice key proxies. Mount under `/_matrix/client/unstable` directly
/// (these paths already spell out `unstable`, unlike everything in [`router`]).
pub fn unstable_router<B: KvBackend + 'static>() -> (axum::Router<E2eState<B>>, RouteManifest) {
    Builder::new()
        .post(
            "/org.matrix.msc3983/keys/claim",
            appservice_proxy::post_msc3983_claim::<B>,
            matrix_appservice("msc3983KeysClaim"),
        )
        .post(
            "/org.matrix.msc3984/keys/query",
            appservice_proxy::post_msc3984_keys_query::<B>,
            matrix_appservice("msc3984KeysQuery"),
        )
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tables::TablesE2eStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hs_auth::state::AuthState;
    use hs_kv::memory::MemoryBackend;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn state() -> E2eState<MemoryBackend> {
        let store = TablesE2eStore::open(MemoryBackend::new()).unwrap();
        E2eState::new(AuthState::in_memory(), Arc::new(store))
    }

    #[tokio::test]
    async fn router_rejects_keys_upload_without_a_token() {
        let (router, _manifest) = router::<MemoryBackend>();
        let app = router.with_state(state());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/keys/upload")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn manifest_covers_every_route_this_module_registers() {
        let (_router, manifest) = router::<MemoryBackend>();
        assert!(manifest.routes.iter().any(|r| r.path == "/keys/upload"));
        assert!(manifest.routes.iter().any(|r| r.path == "/room_keys/version"));
        let (_router2, unstable_manifest) = unstable_router::<MemoryBackend>();
        assert!(
            unstable_manifest
                .routes
                .iter()
                .any(|r| r.path == "/org.matrix.msc3983/keys/claim")
        );
    }
}
