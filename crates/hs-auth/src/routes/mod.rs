//! Legacy Matrix client-server auth endpoints, behavior-compatible with Synapse 1.161
//! (`docs/synapse-inventory.md` is the route checklist; bodies are the spec's).
//!
//! [`router`] returns an axum `Router<AuthState>` with the endpoints at their bare
//! spec-relative paths (`/login`, `/account/whoami`, ...), *not* prefixed with
//! `/_matrix/client/v3`. Prefixing, version aliasing (`r0` vs `v3`) and mounting alongside every
//! other track's routes is `hs-http`'s job (owned jointly with 15 and 14); this crate hands over a
//! router fragment, not a listener.

pub mod account;
pub mod devices;
pub mod login;
pub mod logout;
pub mod refresh;
pub mod register;
pub mod whoami;

use axum::Router;
use axum::routing::{get, post};

use crate::state::AuthState;

/// The legacy auth router fragment. Mount under `/_matrix/client/v3` (and the historical version
/// aliases) with [`axum::Router::nest`] or merge, supplying an [`AuthState`] as the router state.
pub fn router() -> Router<AuthState> {
    Router::new()
        .route(
            "/login",
            get(login::get_login_types).post(login::post_login),
        )
        .route("/logout", post(logout::post_logout))
        .route("/logout/all", post(logout::post_logout_all))
        .route("/refresh", post(refresh::post_refresh))
        .route("/account/whoami", get(whoami::get_whoami))
        .route("/register", post(register::post_register))
        .route("/register/available", get(register::get_register_available))
        .route("/account/password", post(account::post_account_password))
        .route(
            "/account/deactivate",
            post(account::post_account_deactivate),
        )
        .route("/password_policy", get(account::get_password_policy))
        .route("/devices", get(devices::get_devices))
        .route(
            "/devices/{deviceId}",
            get(devices::get_device)
                .put(devices::put_device)
                .delete(devices::delete_device),
        )
        .route("/delete_devices", post(devices::post_delete_devices))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AuthState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn router_serves_get_login_types() {
        let app = router().with_state(AuthState::in_memory());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn router_rejects_whoami_without_a_token() {
        let app = router().with_state(AuthState::in_memory());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/account/whoami")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn router_serves_password_policy_unauthenticated() {
        let app = router().with_state(AuthState::in_memory());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/password_policy")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn full_register_then_whoami_round_trip_through_the_router() {
        let app = router().with_state(AuthState::in_memory());
        let body = serde_json::json!({
            "username": "roundtrip",
            "password": "hunter22",
            "auth": {"type": "m.login.dummy"}
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let access_token = json["access_token"].as_str().unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/account/whoami")
                    .header("Authorization", format!("Bearer {access_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
