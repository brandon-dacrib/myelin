//! What this server answers when no route matches.
//!
//! Axum's own default is a `404` with an empty body, which is not a Matrix error and not an RFC
//! 9457 problem -- a client that asks for an endpoint this server does not have gets a response it
//! cannot parse, and a client library that always decodes the error shape fails on the decode
//! rather than on the status. The Matrix spec is explicit that an unknown endpoint is
//! `404 M_UNRECOGNIZED` and a known path called with the wrong method is `405 M_UNRECOGNIZED`
//! (Complement's `TestUnknownEndpoints` checks both, across the client, federation, key and media
//! prefixes).
//!
//! Which error *shape* to use is decided by the path, because this process serves two APIs with
//! two different error contracts: `/_matrix` and `/_synapse` speak the Matrix `{errcode, error}`
//! shape, and the admin API under [`ADMIN_PREFIX`] speaks RFC 9457 problem details (RFC 0004
//! section 3.5). A fallback that picked one for everything would be wrong for half the server.
//!
//! [`apply`] must be called **last**, on the fully assembled router: axum's
//! `method_not_allowed_fallback` attaches to the `MethodRouter`s registered *before* it, so a
//! route merged in afterwards would silently keep the empty-bodied default.

use axum::Router;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};

use crate::error::MatrixError;
use crate::problem::Problem;

/// The admin API's mount point, whose errors are RFC 9457 problem details rather than Matrix
/// errors.
pub const ADMIN_PREFIX: &str = "/api/v1";

/// Attaches the unknown-endpoint and wrong-method fallbacks to `router`.
///
/// Call this on the finished router, after every route and every merge -- see the module docs for
/// why the order matters.
pub fn apply<S>(router: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        .fallback(unknown_endpoint)
        .method_not_allowed_fallback(unknown_method)
}

/// No route matched this path at all.
async fn unknown_endpoint(request: Request) -> Response {
    if is_admin(request.uri().path()) {
        Problem::not_found()
            .with_detail("no such endpoint")
            .with_instance(request.uri().path())
            .into_response()
    } else {
        MatrixError::unrecognized().into_response()
    }
}

/// The path exists but not for this method. The Matrix spec wants the same errcode as an unknown
/// endpoint -- a client is meant to read `M_UNRECOGNIZED` as "this server does not implement what
/// you asked for", whether the mismatch is in the path or the verb.
async fn unknown_method(request: Request) -> Response {
    if is_admin(request.uri().path()) {
        Problem::method_not_allowed()
            .with_instance(request.uri().path())
            .into_response()
    } else {
        // The allowed-method set is known to axum's router but not handed to this handler, so the
        // response carries no `Allow` list. The status and errcode are what the spec and every
        // client actually key on.
        MatrixError::method_not_allowed(&[]).into_response()
    }
}

fn is_admin(path: &str) -> bool {
    path == ADMIN_PREFIX || path.starts_with(&format!("{ADMIN_PREFIX}/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    fn router() -> Router {
        apply(
            Router::new()
                .route("/_matrix/client/v3/login", get(|| async { "ok" }))
                .route("/api/v1/users", get(|| async { "ok" })),
        )
    }

    async fn call(method: &str, path: &str) -> (StatusCode, serde_json::Value) {
        let response = router()
            .oneshot(
                HttpRequest::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn an_unknown_matrix_endpoint_is_a_json_m_unrecognized() {
        let (status, body) = call("GET", "/_matrix/client/v3/nonsense").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["errcode"], "M_UNRECOGNIZED");
        assert!(
            body["error"].is_string(),
            "the spec's error shape is {{errcode, error}}, and clients decode both"
        );
    }

    /// Complement asks for an unknown prefix (`/_matrix/unknown`) as well as an unknown endpoint
    /// under a known one, because a server that only special-cases the paths it knows about
    /// answers the first with an empty body.
    #[tokio::test]
    async fn an_entirely_unknown_prefix_is_still_a_json_m_unrecognized() {
        for path in [
            "/_matrix/unknown",
            "/_matrix/federation/v1/unknown",
            "/_matrix/key/v2/unknown",
            "/_matrix/media/v3/unknown",
        ] {
            let (status, body) = call("GET", path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(body["errcode"], "M_UNRECOGNIZED", "{path}");
        }
    }

    #[tokio::test]
    async fn a_known_path_with_the_wrong_method_is_405_m_unrecognized() {
        let (status, body) = call("PUT", "/_matrix/client/v3/login").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body["errcode"], "M_UNRECOGNIZED");
    }

    /// The admin API has its own error contract; answering it with a Matrix errcode would break
    /// the one client that reads it -- this project's own management interface.
    #[tokio::test]
    async fn the_admin_api_keeps_problem_details() {
        let (status, body) = call("GET", "/api/v1/nonsense").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            body.get("errcode").is_none(),
            "the admin API speaks RFC 9457, not Matrix errors: {body}"
        );
        assert!(
            body.get("type").is_some(),
            "expected a problem document: {body}"
        );

        let (status, body) = call("POST", "/api/v1/users").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(body.get("errcode").is_none(), "{body}");
    }

    #[tokio::test]
    async fn a_route_that_does_exist_is_untouched() {
        let response = router()
            .oneshot(
                HttpRequest::builder()
                    .uri("/_matrix/client/v3/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
