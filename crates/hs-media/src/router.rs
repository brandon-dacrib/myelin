//! Wires [`crate::routes`] into `hs_http::router::Builder`, producing the authenticated
//! `client/v1/media` router and (when configured) the legacy, unauthenticated `media/v3` router.
//!
//! Paths are spec-relative (`/upload`, not `/_matrix/client/v1/media/upload`) — mounting under the
//! real prefix and composing with the rest of the client listener is the listener's job, matching
//! `hs-auth`'s `routes::router()` convention (`docs/status/07-auth-and-identity.md`'s "Interfaces
//! needed").

use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};
use hs_kv::KvBackend;

use crate::routes;
use crate::state::MediaState;

fn matrix_client(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixClient, AuthKind::Matrix).with_operation_id(operation_id)
}

fn matrix_client_unauthenticated(operation_id: &str) -> RouteMeta {
    RouteMeta::new(Surface::MatrixClient, AuthKind::None).with_operation_id(operation_id)
}

/// The authenticated `client/v1/media` routes: upload (sync and async), download, thumbnail,
/// `/config`.
pub fn authenticated_router<B: KvBackend>() -> (axum::Router<MediaState<B>>, RouteManifest) {
    Builder::new()
        .post(
            "/upload",
            routes::upload::upload_sync::<B>,
            matrix_client("uploadMedia"),
        )
        .post(
            "/create",
            routes::upload::create::<B>,
            matrix_client("createMediaUpload"),
        )
        .put(
            "/upload/{serverName}/{mediaId}",
            routes::upload::put_upload::<B>,
            matrix_client("uploadAsyncMedia"),
        )
        .get(
            "/download/{serverName}/{mediaId}",
            routes::download::download::<B>,
            matrix_client("downloadMedia"),
        )
        .get(
            "/download/{serverName}/{mediaId}/{fileName}",
            routes::download::download_with_filename::<B>,
            matrix_client("downloadMediaWithFilename"),
        )
        .get(
            "/thumbnail/{serverName}/{mediaId}",
            routes::thumbnail::thumbnail::<B>,
            matrix_client("thumbnailMedia"),
        )
        .get(
            "/config",
            routes::config::config::<B>,
            matrix_client("mediaConfig"),
        )
        .get(
            "/preview_url",
            routes::preview::preview_url::<B>,
            matrix_client("previewUrl"),
        )
        .build()
}

/// The genuine MSC2246 async-upload `create` step, at the exact `/_matrix/media/v1/create` path
/// the spec and Synapse use (not `/_matrix/media/v3/...` — an odd but real quirk of Matrix's
/// per-endpoint versioning: this one endpoint was assigned its own `v1` the day it was added,
/// while [`legacy_router`]'s `PUT .../upload/{serverName}/{mediaId}` reused the pre-existing `v3`
/// prefix). **This needs its own mount point** (`hs-cli`'s `merge_router("/_matrix/media/v1",
/// ...)`, not yet wired — see `docs/status/09-media.md`, "Wiring the integration lead must add")
/// — [`Builder`] always serves whatever prefix its caller mounts it under, and no existing mount
/// in this crate's router functions is `/_matrix/media/v1`.
///
/// Deliberately still authenticated (the same reservation-ownership rules as
/// [`routes::upload::create`] itself, and the same "still authenticated" precedent
/// [`legacy_router`]'s own doc states for its upload endpoints) — this is not a new, more
/// permissive route, only the same handler reachable at the path real clients (and Complement)
/// actually call.
pub fn v1_router<B: KvBackend>() -> (axum::Router<MediaState<B>>, RouteManifest) {
    Builder::new()
        .post(
            "/create",
            routes::upload::create::<B>,
            matrix_client("createMediaUploadV1"),
        )
        .build()
}

/// The legacy `media/v3` routes: the same upload endpoints (still authenticated — only download
/// and thumbnail lost their authentication requirement in the legacy path) plus unauthenticated,
/// frozen download and thumbnail handlers (`crate::routes::legacy`).
pub fn legacy_router<B: KvBackend>() -> (axum::Router<MediaState<B>>, RouteManifest) {
    Builder::new()
        .post(
            "/upload",
            routes::upload::upload_sync::<B>,
            matrix_client("legacyUploadMedia"),
        )
        .post(
            "/create",
            routes::upload::create::<B>,
            matrix_client("legacyCreateMediaUpload"),
        )
        .put(
            "/upload/{serverName}/{mediaId}",
            routes::upload::put_upload::<B>,
            matrix_client("legacyUploadAsyncMedia"),
        )
        .get(
            "/download/{serverName}/{mediaId}",
            routes::legacy::legacy_download::<B>,
            matrix_client_unauthenticated("legacyDownloadMedia"),
        )
        .get(
            "/download/{serverName}/{mediaId}/{fileName}",
            routes::legacy::legacy_download_with_filename::<B>,
            matrix_client_unauthenticated("legacyDownloadMediaWithFilename"),
        )
        .get(
            "/thumbnail/{serverName}/{mediaId}",
            routes::legacy::legacy_thumbnail::<B>,
            matrix_client_unauthenticated("legacyThumbnailMedia"),
        )
        .get(
            "/config",
            routes::config::config::<B>,
            matrix_client("legacyMediaConfig"),
        )
        .get(
            "/preview_url",
            routes::preview::preview_url::<B>,
            matrix_client("legacyPreviewUrl"),
        )
        .build()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    /// The bug this session fixes (`docs/status/14-test-and-conformance.md`'s "Track 09 (media)"
    /// gap): `POST /_matrix/media/v1/create` 404s because no mount ever served
    /// `/_matrix/media/v1`. [`v1_router`] itself does answer `/create` correctly — this test
    /// proves that half; `docs/status/09-media.md` records the exact `hs-cli` line still needed
    /// to put it on the wire at the real path.
    #[tokio::test]
    async fn v1_router_serves_create_at_its_relative_path() {
        let (app, state) = crate::test_support::v1_router();
        let token = crate::test_support::seed_token(&state, "@alice:example.org").await;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/create")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["content_uri"]
                .as_str()
                .unwrap()
                .starts_with("mxc://example.org/")
        );
    }
}
