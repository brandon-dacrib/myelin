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
        .build()
}
