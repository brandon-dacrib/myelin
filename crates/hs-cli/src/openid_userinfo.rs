//! `GET /_matrix/federation/v1/openid/userinfo`, answered ahead of the federation router.
//!
//! The spec's OpenID userinfo endpoint is called by a third party (an integration manager, a
//! widget's backend) holding an OpenID token, not by a homeserver: it carries no `X-Matrix`
//! signature, and none can be asked of it. `hs-federation` registers the route inside its router,
//! behind the `X-Matrix` layer every other federation route sits behind, so every real request
//! was refused `401 "signature verification failed"` before the handler could look at the token
//! -- including Sytest's "Can generate a openid access_token that can be exchanged for
//! information about a user", and, for the wrong reason, the "invalid token" tests that passed.
//! The `/3pid/onbind` route had the same problem and is mounted outside the layer by
//! `crate::serve`; this one cannot be, because the path is already taken inside the federation
//! router (an axum route cannot be registered twice), so this middleware answers it before
//! routing. The right fix is in `hs-federation` (register the route outside the layer, as the
//! spec has it), after which this module goes.
//!
//! Tokens are `hs_auth::openid`'s. A missing token is `401 M_MISSING_TOKEN`, an unknown or
//! expired one `401 M_UNKNOWN_TOKEN`, as Synapse answers.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// The path this middleware answers.
pub const PATH: &str = "/_matrix/federation/v1/openid/userinfo";

/// Answers `GET` [`PATH`] from the auth store; every other request goes on to the router.
pub async fn ahead_of_x_matrix(
    State(auth): State<hs_auth::state::AuthState>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::GET || request.uri().path() != PATH {
        return next.run(request).await;
    }
    let token = axum::extract::Query::<std::collections::HashMap<String, String>>::try_from_uri(
        request.uri(),
    )
    .ok()
    .and_then(|axum::extract::Query(mut query)| query.remove("access_token"));
    let Some(token) = token else {
        return error(
            StatusCode::UNAUTHORIZED,
            "M_MISSING_TOKEN",
            "Missing access_token",
        );
    };
    match hs_auth::openid::userinfo(auth.store.as_ref(), &token, auth.now_ms()).await {
        Ok(Some(user_id)) => axum::Json(json!({ "sub": user_id })).into_response(),
        Ok(None) => error(
            StatusCode::UNAUTHORIZED,
            "M_UNKNOWN_TOKEN",
            "Invalid or expired OpenID token",
        ),
        Err(err) => {
            tracing::warn!(error = %err, "could not look up an OpenID token");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "M_UNKNOWN",
                "Internal error",
            )
        }
    }
}

fn error(status: StatusCode, errcode: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"errcode": errcode, "error": message})),
    )
        .into_response()
}
