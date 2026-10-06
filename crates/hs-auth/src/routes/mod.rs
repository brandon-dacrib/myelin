//! Legacy Matrix client-server auth endpoints, behavior-compatible with Synapse 1.161
//! (`docs/synapse-inventory.md` is the route checklist; bodies are the spec's).
//!
//! [`router`] returns an axum `Router<AuthState>` with the endpoints at their bare
//! spec-relative paths (`/login`, `/account/whoami`, ...), *not* prefixed with
//! `/_matrix/client/v3`. Prefixing, version aliasing (`r0` vs `v3`) and mounting alongside every
//! other track's routes is `hs-http`'s job (owned jointly with 15 and 14); this crate hands over a
//! router fragment, not a listener.
//!
//! [`synapse_admin`] is a separate router fragment at an absolute, non-`/_matrix` path
//! (`/_synapse/admin/v1/register`) and is deliberately **not** part of [`router`]'s fragment --
//! see that module's doc comment for why. [`crate::synapse_admin_router`] re-exports it at the
//! crate root for callers that do not want to reach into `routes::synapse_admin` directly.

pub mod account;
pub mod devices;
pub mod login;
pub mod logout;
pub mod profile;
pub mod refresh;
pub mod register;
pub mod sso;
pub mod synapse_admin;
pub mod threepid;
pub mod user_directory;
pub mod whoami;
pub mod whois;

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
        .route("/login/sso/redirect", get(sso::get_sso_redirect))
        .route(
            "/login/sso/redirect/{idpId}",
            get(sso::get_sso_redirect_idp),
        )
        .route("/login/cas/redirect", get(sso::get_sso_redirect))
        .route("/login/cas/ticket", get(sso::get_cas_ticket))
        .route("/auth/m.login.sso/fallback/web", get(sso::get_sso_fallback))
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
        // A user's own third-party identifiers (`crate::threepid`): listed, added once this
        // server has validated them, deleted, and bound or unbound at an identity server.
        .route(
            "/account/3pid",
            get(account::get_account_3pid).post(threepid::post_account_3pid),
        )
        .route("/account/3pid/add", post(threepid::post_account_3pid_add))
        .route(
            "/account/3pid/delete",
            post(threepid::post_account_3pid_delete),
        )
        .route("/account/3pid/bind", post(threepid::post_account_3pid_bind))
        .route(
            "/account/3pid/unbind",
            post(threepid::post_account_3pid_unbind),
        )
        .route(
            "/register/email/requestToken",
            post(threepid::post_register_email_request_token),
        )
        .route(
            "/account/3pid/email/requestToken",
            post(threepid::post_account_3pid_email_request_token),
        )
        .route(
            "/account/password/email/requestToken",
            post(threepid::post_password_email_request_token),
        )
        .route(
            "/register/msisdn/requestToken",
            post(threepid::post_msisdn_request_token),
        )
        .route(
            "/account/3pid/msisdn/requestToken",
            post(threepid::post_msisdn_request_token),
        )
        .route(
            "/account/password/msisdn/requestToken",
            post(threepid::post_msisdn_request_token),
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
        .route(
            "/user_directory/search",
            post(user_directory::post_user_directory_search),
        )
        .route("/profile/{userId}", get(profile::get_profile))
        // `PUT` for both of these lives in `hs-room`'s router instead
        // (`crates/hs-room/src/routes/profile.rs`), merged at the same prefix in `hs-cli`'s
        // `serve.rs` -- see that module's doc comment for why. It calls straight back into
        // `profile::put_displayname`/`put_avatar_url` below for the actual store write, so those
        // functions stay exactly as they were, just no longer reachable from *this* crate's own
        // router.
        .route(
            "/profile/{userId}/displayname",
            get(profile::get_displayname),
        )
        .route("/profile/{userId}/avatar_url", get(profile::get_avatar_url))
        .route(
            "/user/{userId}/openid/request_token",
            post(crate::openid::post_request_token),
        )
        .route("/admin/whois/{userId}", get(whois::get_whois))
}

/// The auth endpoints served under `/_matrix/client/unstable`: the links in validation emails
/// (`/<purpose>/email/submit_token`, where Synapse serves them) and the `/account/3pid/*`
/// routes, which Synapse also answers under `unstable` and Sytest calls there
/// (`/unstable/account/3pid/bind`). Mount under `/_matrix/client/unstable` only.
pub fn unstable_router() -> Router<AuthState> {
    Router::new()
        .route(
            "/registration/email/submit_token",
            get(threepid::get_registration_submit_token),
        )
        .route(
            "/add_threepid/email/submit_token",
            get(threepid::get_add_threepid_submit_token),
        )
        .route(
            "/password_reset/email/submit_token",
            get(threepid::get_password_reset_submit_token)
                .post(threepid::post_password_reset_submit_token),
        )
        .route("/account/3pid/add", post(threepid::post_account_3pid_add))
        .route(
            "/account/3pid/delete",
            post(threepid::post_account_3pid_delete),
        )
        .route("/account/3pid/bind", post(threepid::post_account_3pid_bind))
        .route(
            "/account/3pid/unbind",
            post(threepid::post_account_3pid_unbind),
        )
}

/// The auth endpoints the spec defines under `/_matrix/client/v1` rather than `v3`: today, the
/// registration-token validity check. Mount under `/_matrix/client/v1` only.
pub fn v1_router() -> Router<AuthState> {
    Router::new().route(
        "/register/m.login.registration_token/validity",
        get(register::get_registration_token_validity),
    )
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

    /// The route existing at all is the fix: Element's Settings page calls it on open and shows
    /// the user a visible error when it 404s. Goes through the router with a real token rather
    /// than calling the handler, because "the handler compiles" was never the missing part.
    #[tokio::test]
    async fn router_serves_account_3pid_to_a_registered_user() {
        let app = router().with_state(AuthState::in_memory());
        let body = serde_json::json!({
            "username": "threepid",
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
                    .uri("/account/3pid")
                    .header("Authorization", format!("Bearer {access_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["threepids"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn router_rejects_account_3pid_without_a_token() {
        let app = router().with_state(AuthState::in_memory());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/account/3pid")
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
