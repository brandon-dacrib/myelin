//! `GET /_matrix/federation/v1/openid/userinfo`: the one route under the federation prefix that
//! is not called by a homeserver, and so carries no `X-Matrix` signature. An integration manager
//! or a widget's backend calls it with the OpenID token a user handed it
//! (`POST /_matrix/client/v3/user/{userId}/openid/request_token`) to learn whose it is.
//!
//! It is its own router, not part of [`super::router`], because it belongs to the client side
//! of a server as much as to federation: Synapse serves it from its `openid` listener resource,
//! which a deployment may enable without `federation` (a server that does not federate still
//! hands its users OpenID tokens for an integration manager). `hs serve` mounts it whenever the
//! client listener runs, whatever `federation.enabled` says. Before 2026-10-05 it sat inside the
//! `X-Matrix` layer and every real call was refused; after, it was merged into the federation
//! router beside that layer, so `federation.enabled: false` stopped serving it.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use hs_http::error::MatrixError;
use hs_http::router::{AuthKind, Builder, RouteManifest, RouteMeta, Surface};

/// Resolves OpenID tokens for [`router`].
#[async_trait]
pub trait OpenIdUserinfoSource: Send + Sync {
    /// The local Matrix user ID a live OpenID access token belongs to; `None` for a token that
    /// is unknown or expired.
    async fn openid_userinfo(&self, access_token: &str) -> Option<String>;
}

/// The `/openid/userinfo` route, spec-relative: the caller mounts it under
/// `/_matrix/federation/v1`. Unauthenticated in the manifest ([`AuthKind::None`]): the token in
/// the query string is the whole of what it checks.
pub fn router(source: Arc<dyn OpenIdUserinfoSource>) -> (axum::Router, RouteManifest) {
    let (router, manifest) = Builder::<Arc<dyn OpenIdUserinfoSource>>::new()
        .get(
            "/openid/userinfo",
            openid_userinfo,
            RouteMeta::new(Surface::MatrixFederation, AuthKind::None)
                .with_operation_id("federationOpenIdUserinfo"),
        )
        .build();
    (router.with_state(source), manifest)
}

/// `GET /openid/userinfo?access_token=`: `{"sub": user_id}` for a live OpenID token. A missing
/// token is `401 M_MISSING_TOKEN` and an unknown or expired one `401 M_UNKNOWN_TOKEN`, as
/// Synapse's `OpenIdUserInfo` answers.
async fn openid_userinfo(
    State(source): State<Arc<dyn OpenIdUserinfoSource>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(token) = params.get("access_token") else {
        return MatrixError::missing_token().into_response();
    };
    match source.openid_userinfo(token).await {
        Some(user_id) => axum::Json(serde_json::json!({ "sub": user_id })).into_response(),
        None => MatrixError::unknown_token("Access Token unknown or expired").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::InMemoryQuerySource;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// An unsigned call with a live token gets the user; without a token or with an unknown
    /// one, the spec's `401` codes. The route is the manifest's one unauthenticated entry.
    #[tokio::test]
    async fn openid_userinfo_answers_an_unsigned_request() {
        let queries = Arc::new(InMemoryQuerySource::default());
        queries.insert_openid_token("opaque", "@alice:us.example.org");
        let (router, manifest) = router(queries);
        let paths: Vec<(&str, AuthKind)> = manifest
            .routes
            .iter()
            .map(|r| (r.path.as_str(), r.auth))
            .collect();
        assert_eq!(paths, [("/openid/userinfo", AuthKind::None)]);
        for (uri, status, body) in [
            (
                "/openid/userinfo?access_token=opaque",
                StatusCode::OK,
                serde_json::json!({"sub": "@alice:us.example.org"}),
            ),
            (
                "/openid/userinfo",
                StatusCode::UNAUTHORIZED,
                serde_json::json!("M_MISSING_TOKEN"),
            ),
            (
                "/openid/userinfo?access_token=nope",
                StatusCode::UNAUTHORIZED,
                serde_json::json!("M_UNKNOWN_TOKEN"),
            ),
        ] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{uri}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if status == StatusCode::OK {
                assert_eq!(json, body, "{uri}");
            } else {
                assert_eq!(json["errcode"], body, "{uri}");
            }
        }
    }
}
