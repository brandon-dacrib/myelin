//! The join/leave/knock/invite handshakes, `/send`, and every other federation endpoint this
//! track has not built real logic for yet. Per
//! `docs/design/06-federation-threat-model.md` section 2.5: each of these routes sits behind the
//! same `X-Matrix` verification layer as every other route in this router (see
//! `crate::transport::router`), and its handler does nothing beyond that — no partial logic that
//! could be mistaken for a working implementation. A half-built handshake handler is more
//! dangerous than an honest "not implemented", because a peer (or a future contributor) could
//! mistake it for working.
//!
//! `user/keys/claim` and `user/keys/query` are explicitly joint-owned with track 08 (E2EE, not
//! yet started); they are mounted here as seams like everything else in this module until that
//! track defines the real contract. The media endpoints and the two `_synapse/client/*` compat
//! entries from `docs/synapse-inventory.md` are deliberately **not** mounted here at all (media is
//! track 09's; the `_synapse/client/*` pair is client-prefixed compat surface, not
//! server-to-server).

use axum::body::Bytes;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::error::{MatrixError, MatrixErrorCode};
use hs_http::router::Builder;

use crate::transport::FederationState;

pub(super) fn add_routes(builder: Builder<FederationState>) -> Builder<FederationState> {
    fn meta(op: &str) -> hs_http::router::RouteMeta {
        super::matrix_federation(op)
    }

    macro_rules! seam {
        ($builder:expr, $method:expr, $path:literal, $op:literal) => {
            $builder.add($method, $path, not_implemented, meta($op))
        };
    }

    let mut builder = builder;
    builder = seam!(builder, Method::PUT, "/send/{txnId}", "federationSend");
    builder = seam!(
        builder,
        Method::GET,
        "/make_join/{roomId}/{userId}",
        "federationMakeJoin"
    );
    builder = seam!(
        builder,
        Method::PUT,
        "/send_join/{roomId}/{eventId}",
        "federationSendJoinV1"
    );
    builder = seam!(
        builder,
        Method::PUT,
        "/send_join/v2/{roomId}/{eventId}",
        "federationSendJoinV2"
    );
    builder = seam!(
        builder,
        Method::GET,
        "/make_leave/{roomId}/{userId}",
        "federationMakeLeave"
    );
    builder = seam!(
        builder,
        Method::PUT,
        "/send_leave/{roomId}/{eventId}",
        "federationSendLeaveV1"
    );
    builder = seam!(
        builder,
        Method::PUT,
        "/send_leave/v2/{roomId}/{eventId}",
        "federationSendLeaveV2"
    );
    builder = seam!(
        builder,
        Method::GET,
        "/make_knock/{roomId}/{userId}",
        "federationMakeKnock"
    );
    builder = seam!(
        builder,
        Method::PUT,
        "/send_knock/{roomId}/{eventId}",
        "federationSendKnock"
    );
    builder = seam!(
        builder,
        Method::PUT,
        "/invite/{roomId}/{eventId}",
        "federationInviteV1"
    );
    builder = seam!(
        builder,
        Method::PUT,
        "/invite/v2/{roomId}/{eventId}",
        "federationInviteV2"
    );
    builder = seam!(
        builder,
        Method::PUT,
        "/exchange_third_party_invite/{roomId}",
        "federationExchangeThirdPartyInvite"
    );
    // `PUT`, not `POST`: `third_party_invite.yaml`'s `onBindThirdPartyIdentifier` is a PUT, and a
    // seam registered under the wrong method is a 404 to the only caller that would ever use it.
    builder = seam!(
        builder,
        Method::PUT,
        "/3pid/onbind",
        "federationThreepidOnbind"
    );
    builder = seam!(
        builder,
        Method::POST,
        "/user/keys/claim",
        "federationUserKeysClaim"
    );
    builder = seam!(
        builder,
        Method::POST,
        "/user/keys/query",
        "federationUserKeysQuery"
    );
    builder = seam!(
        builder,
        Method::GET,
        "/rooms/{roomId}/complexity",
        "federationRoomComplexity"
    );
    builder = seam!(
        builder,
        Method::GET,
        "/extremities/{roomId}",
        "federationExtremities"
    );
    builder = seam!(
        builder,
        Method::GET,
        "/query/account_status",
        "federationQueryAccountStatus"
    );

    builder
}

/// The shared seam handler: bounded (by the `X-Matrix` layer's own body cap) body is accepted and
/// discarded; the response is always a clear, typed "not implemented" error. Reused across every
/// route registered above rather than one function per route, since they all do exactly this.
async fn not_implemented(_body: Bytes) -> Response {
    MatrixError::custom(
        StatusCode::NOT_IMPLEMENTED,
        MatrixErrorCode::Other("M_NOT_IMPLEMENTED".to_string()),
        "this endpoint is not implemented yet",
    )
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_source::InMemoryRoomSource;
    use crate::transport::InMemoryQuerySource;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn state() -> FederationState {
        FederationState {
            own_server_name: Arc::from("us.example.org"),
            rooms: Arc::new(InMemoryRoomSource::new()),
            queries: Arc::new(InMemoryQuerySource::default()),
            allow_public_rooms_over_federation: false,
            allow_device_name_lookup_over_federation: false,
        }
    }

    fn build() -> (axum::Router<FederationState>, Vec<hs_http::router::Route>) {
        let (router, manifest) = add_routes(Builder::<FederationState>::new()).build();
        (router, manifest.routes)
    }

    #[tokio::test]
    async fn send_is_a_clean_not_implemented_seam() {
        let (router, _routes) = build();
        let response = router
            .with_state(state())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/send/1")
                    .body(Body::from(r#"{"pdus": [], "edus": []}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn every_seam_route_responds_not_implemented() {
        let (router, routes) = build();
        for route in routes {
            let path = route
                .path
                .split('/')
                .map(|seg| if seg.starts_with('{') { "x" } else { seg })
                .collect::<Vec<_>>()
                .join("/");
            let response = router
                .clone()
                .with_state(state())
                .oneshot(
                    Request::builder()
                        .method(route.method.as_str())
                        .uri(&path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_IMPLEMENTED,
                "route {} {} did not respond 501",
                route.method,
                route.path
            );
        }
    }
}
