//! A single-stage UIA "re-authenticate as yourself" check, shared by every endpoint that mutates
//! something sensitive about an already-logged-in account: `POST /account/password`,
//! `POST /account/deactivate`, `DELETE /devices/{deviceId}` and `POST /delete_devices`.
//!
//! The flow is `m.login.password` (re-enter the current password) when the account has one, or
//! `m.login.dummy` when it does not (SSO-only or appservice-provisioned accounts have nothing to
//! re-check, so acknowledgement is all UIA can usefully ask for). Synapse additionally offers a
//! short grace period after a recent login where UIA is skipped entirely; this server always
//! requires it — a deliberate simplification, noted in
//! `docs/rfcs/0002-auth-tokens-and-requester.md` section 6.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ruma::api::client::uiaa::{AuthData, AuthFlow, AuthType};
use serde_json::Value;

use crate::error::MatrixError;
use crate::password;
use crate::requester::Requester;
use crate::state::AuthState;
use crate::uia;

fn flow(has_password: bool) -> Vec<AuthFlow> {
    if has_password {
        vec![AuthFlow::new(vec![AuthType::Password])]
    } else {
        vec![AuthFlow::new(vec![AuthType::Dummy])]
    }
}

async fn verify(
    state: &AuthState,
    requester: &Requester,
    data: &AuthData,
) -> Result<bool, MatrixError> {
    match data {
        AuthData::Dummy(_) => Ok(true),
        AuthData::Password(p) => {
            let user = state.store.get_user(&requester.user_id).await?;
            match user.and_then(|u| u.password_hash) {
                Some(hash) => {
                    Ok(
                        password::verify_password(&p.password, &hash, &state.config.bcrypt_pepper)
                            .unwrap_or(false),
                    )
                }
                None => Ok(false),
            }
        }
        _ => Ok(false),
    }
}

/// Runs one round of the re-auth flow against `body`'s `auth` field. Returns `Ok(None)` once the
/// flow is complete (the caller should proceed with its sensitive operation) or
/// `Ok(Some(response))` — a `401` with the `UiaaInfo` body — when another round is needed.
pub async fn run(
    state: &AuthState,
    requester: &Requester,
    body: &Value,
) -> Result<Option<Response>, MatrixError> {
    let user = state
        .store
        .get_user(&requester.user_id)
        .await?
        .ok_or_else(MatrixError::internal)?;
    let flows = flow(user.password_hash.is_some());

    let auth: Option<AuthData> = match body.get("auth") {
        Some(v) if !v.is_null() => Some(
            serde_json::from_value(v.clone())
                .map_err(|_| MatrixError::invalid_param("invalid auth data"))?,
        ),
        _ => None,
    };
    let session_id = auth.as_ref().and_then(AuthData::session);
    let submitted_type = auth.as_ref().and_then(AuthData::auth_type);
    let stage_ok = match &auth {
        Some(data) => verify(state, requester, data).await?,
        None => true,
    };

    let outcome = uia::advance(
        state.store.as_ref(),
        &flows,
        session_id,
        submitted_type,
        stage_ok,
        state.now_ms(),
        state.config.uia_session_timeout_ms,
    )
    .await?;

    if outcome.complete {
        Ok(None)
    } else {
        let body = uia::incomplete_body(flows, outcome.completed, outcome.session_id);
        Ok(Some((StatusCode::UNAUTHORIZED, Json(body)).into_response()))
    }
}
