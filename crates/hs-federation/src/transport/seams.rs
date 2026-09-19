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
    // `/send`, `make_join` and the v1 `send_join` are real now — see `crate::transport::send` and
    // `crate::transport::join`. The v2 `send_join`/`send_leave`/`invite` spellings are registered
    // by `add_routes_v2` below, for a router mounted separately at `/_matrix/federation/v2` (they
    // used to sit here, wrongly, under a literal `/v2/` path segment — see this crate's status
    // file).
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

/// The v2-mount seams: `send_leave` and `invite`'s v2 spellings, registered as
/// `/send_leave/{roomId}/{eventId}` and `/invite/{roomId}/{eventId}` for a router mounted
/// separately at `/_matrix/federation/v2` (see `crate::transport::router_v2`'s doc — the path
/// string is identical to the v1 seams above; only the mount prefix differs, which is exactly why
/// these could not previously share one `Builder` with the v1 spellings without colliding).
pub(super) fn add_routes_v2(builder: Builder<FederationState>) -> Builder<FederationState> {
    fn meta(op: &str) -> hs_http::router::RouteMeta {
        super::matrix_federation(op)
    }
    builder
        .add(
            Method::PUT,
            "/send_leave/{roomId}/{eventId}",
            not_implemented,
            meta("federationSendLeaveV2"),
        )
        .add(
            Method::PUT,
            "/invite/{roomId}/{eventId}",
            not_implemented,
            meta("federationInviteV2"),
        )
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
            write_sink: Arc::new(crate::inbound::StaticWriteSink::new(
                Vec::new(),
                "not supported",
            )),
            transactions: Arc::new(crate::inbound::InMemoryTransactionStore::new()),
            ancestor_fetcher: None,
            backfill_limits: crate::backfill::BackfillLimits::default(),
        }
    }

    fn build() -> (axum::Router<FederationState>, Vec<hs_http::router::Route>) {
        let (router, manifest) = add_routes(Builder::<FederationState>::new()).build();
        (router, manifest.routes)
    }

    fn build_v2() -> (axum::Router<FederationState>, Vec<hs_http::router::Route>) {
        let (router, manifest) = add_routes_v2(Builder::<FederationState>::new()).build();
        (router, manifest.routes)
    }

    async fn assert_every_route_is_a_clean_seam(
        router: axum::Router<FederationState>,
        routes: Vec<hs_http::router::Route>,
    ) {
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

    #[tokio::test]
    async fn every_seam_route_responds_not_implemented() {
        let (router, routes) = build();
        assert_every_route_is_a_clean_seam(router, routes).await;
    }

    #[tokio::test]
    async fn every_v2_seam_route_responds_not_implemented() {
        let (router, routes) = build_v2();
        assert!(!routes.is_empty());
        assert_every_route_is_a_clean_seam(router, routes).await;
    }

    /// `/send`, `make_join` and the v1 `send_join` used to be seams registered by this module;
    /// this proves they are gone from here (they now live in `crate::transport::send` and
    /// `crate::transport::join`), so nobody accidentally re-adds a seam for a route this crate now
    /// implements for real.
    #[tokio::test]
    async fn the_real_write_routes_are_not_registered_as_seams_here() {
        let (_, routes) = build();
        let paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
        assert!(!paths.contains(&"/send/{txnId}"));
        assert!(!paths.contains(&"/make_join/{roomId}/{userId}"));
        assert!(!paths.contains(&"/send_join/{roomId}/{eventId}"));
    }
}
