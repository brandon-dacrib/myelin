//! `GET`/`POST /_synapse/admin/v1/register`: Synapse's shared-secret admin registration protocol
//! (`register_new_matrix_user`-compatible; see `hs_compat::shared_secret` for the wire format,
//! cross-checked against Synapse's documented vectors).
//!
//! [`router`] returns its own router fragment at the **absolute** path
//! `/_synapse/admin/v1/register` -- unlike [`super::router`], which returns spec-relative paths
//! meant to be nested under `/_matrix/client/v3` and its version aliases, this one is a
//! Synapse-compatibility path rooted at the server root and must be mounted as-is, not nested
//! under any prefix. It is therefore *not* merged into [`super::router`]; `hs serve` mounts both
//! fragments independently. See `docs/status/07-auth-and-identity.md` for the exact `hs-cli`
//! wiring line this replaces (today, nothing mounts it at all, so `hs register --admin` 404s).
//!
//! # Why the secret has to be configured
//!
//! Both routes answer `404 M_UNRECOGNIZED` (via [`crate::error::MatrixError::feature_not_configured`])
//! when [`crate::config::AuthConfig::registration_shared_secret`] is unset, never a `500` and
//! never a working-but-pointless nonce: an operator who has not opted into this endpoint should
//! not be able to tell it apart from a route that was never registered.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::Router;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hs_compat::shared_secret::{
    NonceError, NonceRegistry, RegistrationError, RegistrationRequest, verify_registration_request,
};
use ruma::UserId;
use serde_json::{Map, Value, json};

use crate::error::{ErrCode, MatrixError};
use crate::password;
use crate::session;
use crate::state::AuthState;
use crate::store::UserRecord;

/// The [`NonceRegistry`] this router fragment's two routes share, injected with
/// [`axum::Extension`] rather than carried in [`AuthState`].
///
/// It lives here, not on `AuthState`, because it is single-process, in-memory, short-lived (60s
/// per nonce, see [`hs_compat::shared_secret::NONCE_TIMEOUT`]) bookkeeping for exactly this one
/// endpoint pair -- there is no reason for every other handler in this crate to carry it around
/// in shared state it never reads. A clustered deployment that needs `POST /_synapse/admin/v1/
/// register` to keep working routes it to whichever single replica owns this router fragment's
/// instance (matching how Synapse's own single-writer nonce cache behaves; see
/// `hs_compat::shared_secret::NonceRegistry`'s own doc comment) -- that is an `hs-cluster`/routing
/// concern, not something this crate's state shape should force on every other handler.
type SharedNonceRegistry = Arc<Mutex<NonceRegistry>>;

/// The shared-secret admin-registration router fragment. Mount at the server root (not nested
/// under `/_matrix/client/v3`) with [`axum::Router::merge`].
pub fn router() -> Router<AuthState> {
    let registry: SharedNonceRegistry = Arc::new(Mutex::new(NonceRegistry::new()));
    Router::new()
        .route(
            "/_synapse/admin/v1/register",
            get(get_register_nonce).post(post_register),
        )
        .layer(Extension(registry))
}

async fn get_register_nonce(
    State(state): State<AuthState>,
    Extension(registry): Extension<SharedNonceRegistry>,
) -> Result<Json<Value>, MatrixError> {
    if state.config.registration_shared_secret.is_none() {
        return Err(MatrixError::feature_not_configured());
    }
    let nonce = registry
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .issue();
    Ok(Json(json!({ "nonce": nonce })))
}

async fn post_register(
    State(state): State<AuthState>,
    Extension(registry): Extension<SharedNonceRegistry>,
    Json(body): Json<Value>,
) -> Result<Response, MatrixError> {
    let Some(secret) = state.config.registration_shared_secret.clone() else {
        return Err(MatrixError::feature_not_configured());
    };

    let req = parse_registration_request(&body)?;

    // Verify (and consume) the nonce and MAC before touching the store at all: this is the
    // request's *authentication*, and nothing about it -- including whether the requested
    // username is taken -- should be observable to a caller who has not passed it.
    {
        let mut reg = registry.lock().unwrap_or_else(PoisonError::into_inner);
        verify_registration_request(&mut reg, secret.as_bytes(), &req)
            .map_err(map_registration_error)?;
    }

    state.config.password_policy.validate(&req.password)?;

    let user_id = UserId::parse_with_server_name(req.username.as_str(), state.server_name())
        .map_err(|_| {
            MatrixError::invalid_username(format!(
                "'{}' is not a valid user ID localpart",
                req.username
            ))
        })?;

    if !state
        .store
        .is_localpart_available(user_id.localpart())
        .await?
    {
        return Err(MatrixError::user_in_use());
    }

    let password_hash =
        password::hash_password(&req.password).map_err(|_| MatrixError::internal())?;

    // Defensive re-check, the same TOCTOU-closing pattern `routes::register.rs::register_user`
    // uses between its own early availability check and account creation.
    if !state
        .store
        .is_localpart_available(user_id.localpart())
        .await?
    {
        return Err(MatrixError::user_in_use());
    }

    state
        .store
        .create_user({
            let mut r = UserRecord::new(user_id.clone(), state.now_ms());
            r.password_hash = Some(password_hash);
            r.is_admin = req.admin;
            r
        })
        .await?;

    // No UIA: the MAC verification above *is* this endpoint's authentication.
    let session = session::create_session(&state, &user_id, None, None, false).await?;

    Ok(Json(json!({
        "user_id": user_id,
        "access_token": session.access_token,
        "home_server": state.server_name().as_str(),
        "device_id": session.device_id,
    }))
    .into_response())
}

fn parse_registration_request(body: &Value) -> Result<RegistrationRequest, MatrixError> {
    let obj: &Map<String, Value> = body.as_object().ok_or_else(|| {
        MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::BadJson,
            "request body must be a JSON object",
        )
    })?;
    let field = |name: &str| -> Result<String, MatrixError> {
        obj.get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| MatrixError::missing_param(format!("Missing {name}")))
    };
    Ok(RegistrationRequest {
        nonce: field("nonce")?,
        username: field("username")?,
        password: field("password")?,
        admin: obj.get("admin").and_then(Value::as_bool).unwrap_or(false),
        user_type: obj
            .get("user_type")
            .and_then(Value::as_str)
            .map(String::from),
        mac: field("mac")?,
    })
}

/// Maps a MAC-verification failure onto a Matrix error, matching Synapse's observable status
/// codes for the same two failure modes: a bad/replayed/expired nonce is `400`, a MAC that does
/// not match is `403` (the caller does not know the secret).
fn map_registration_error(err: RegistrationError) -> MatrixError {
    match err {
        RegistrationError::Nonce(NonceError::Unknown) => MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::Unknown,
            "unrecognised nonce",
        ),
        RegistrationError::Nonce(NonceError::Expired) => {
            MatrixError::new(StatusCode::BAD_REQUEST, ErrCode::Unknown, "nonce expired")
        }
        RegistrationError::Mac(_) => {
            MatrixError::new(StatusCode::FORBIDDEN, ErrCode::Forbidden, "HMAC incorrect")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthConfig;
    use axum::body::Body;
    use axum::http::Request;
    use hs_compat::shared_secret::compute_mac;
    use tower::ServiceExt;

    const SECRET: &str = "shared-secret";

    fn state_with_secret() -> AuthState {
        AuthState::in_memory_with_config(AuthConfig {
            registration_shared_secret: Some(SECRET.to_string()),
            ..AuthConfig::default()
        })
    }

    async fn get_nonce(app: &Router<()>) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/_synapse/admin/v1/register")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post(app: &Router<()>, body: Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_synapse/admin/v1/register")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn nonce_then_register_completes_and_grants_admin() {
        let state = state_with_secret();
        let app: Router<()> = router().with_state(state.clone());

        let (status, nonce_body) = get_nonce(&app).await;
        assert_eq!(status, StatusCode::OK);
        let nonce = nonce_body["nonce"].as_str().unwrap().to_string();

        let mac = compute_mac(
            SECRET.as_bytes(),
            &nonce,
            "newadmin",
            "hunter22pass",
            true,
            None,
        );
        let body = json!({
            "nonce": nonce,
            "username": "newadmin",
            "password": "hunter22pass",
            "admin": true,
            "mac": mac,
        });
        let (status, response) = post(&app, body).await;
        assert_eq!(status, StatusCode::OK, "response was {response:?}");
        assert_eq!(response["user_id"], "@newadmin:example.org");
        assert_eq!(response["home_server"], "example.org");
        assert!(response["access_token"].as_str().is_some());
        assert!(response["device_id"].as_str().is_some());

        let user = state
            .store
            .get_user(ruma::user_id!("@newadmin:example.org"))
            .await
            .unwrap()
            .unwrap();
        assert!(user.is_admin);
    }

    #[tokio::test]
    async fn both_routes_404_unrecognized_when_secret_not_configured() {
        let app: Router<()> = router().with_state(AuthState::in_memory());

        let (status, body) = get_nonce(&app).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["errcode"], "M_UNRECOGNIZED");

        let (status, body) = post(
            &app,
            json!({"nonce": "x", "username": "x", "password": "x", "admin": false, "mac": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["errcode"], "M_UNRECOGNIZED");
    }

    #[tokio::test]
    async fn replayed_nonce_is_rejected() {
        let app: Router<()> = router().with_state(state_with_secret());

        let (_, nonce_body) = get_nonce(&app).await;
        let nonce = nonce_body["nonce"].as_str().unwrap().to_string();
        let mac = compute_mac(
            SECRET.as_bytes(),
            &nonce,
            "replay",
            "hunter22pass",
            false,
            None,
        );
        let body = json!({
            "nonce": nonce,
            "username": "replay",
            "password": "hunter22pass",
            "admin": false,
            "mac": mac,
        });

        let (status, _) = post(&app, body.clone()).await;
        assert_eq!(status, StatusCode::OK);

        // Same nonce again: already consumed.
        let (status, response) = post(&app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["errcode"], "M_UNKNOWN");
    }

    #[tokio::test]
    async fn bad_mac_is_rejected() {
        let app: Router<()> = router().with_state(state_with_secret());

        let (_, nonce_body) = get_nonce(&app).await;
        let nonce = nonce_body["nonce"].as_str().unwrap().to_string();
        let body = json!({
            "nonce": nonce,
            "username": "badmac",
            "password": "hunter22pass",
            "admin": false,
            "mac": "0000000000000000000000000000000000000000",
        });

        let (status, response) = post(&app, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(response["errcode"], "M_FORBIDDEN");
    }

    #[tokio::test]
    async fn taken_username_is_rejected() {
        let app: Router<()> = router().with_state(state_with_secret());

        let (_, nonce_body) = get_nonce(&app).await;
        let nonce = nonce_body["nonce"].as_str().unwrap().to_string();
        let mac = compute_mac(
            SECRET.as_bytes(),
            &nonce,
            "dupe",
            "hunter22pass",
            false,
            None,
        );
        let body = json!({
            "nonce": nonce,
            "username": "dupe",
            "password": "hunter22pass",
            "admin": false,
            "mac": mac,
        });
        let (status, _) = post(&app, body).await;
        assert_eq!(status, StatusCode::OK);

        let (_, nonce_body) = get_nonce(&app).await;
        let nonce = nonce_body["nonce"].as_str().unwrap().to_string();
        let mac = compute_mac(
            SECRET.as_bytes(),
            &nonce,
            "dupe",
            "otherpassword",
            false,
            None,
        );
        let body = json!({
            "nonce": nonce,
            "username": "dupe",
            "password": "otherpassword",
            "admin": false,
            "mac": mac,
        });
        let (status, response) = post(&app, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["errcode"], "M_USER_IN_USE");
    }
}
