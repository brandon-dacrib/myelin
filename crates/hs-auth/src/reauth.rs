//! A single-stage UIA "re-authenticate as yourself" check, shared by every endpoint that mutates
//! something sensitive about an already-logged-in account: `POST /account/password`,
//! `POST /account/deactivate`, `DELETE /devices/{deviceId}` and `POST /delete_devices`.
//!
//! The flows are `m.login.password` (re-enter the current password) when the account has one, or
//! `m.login.dummy` when it does not (SSO-only or appservice-provisioned accounts have nothing to
//! re-check, so acknowledgement is all UIA can usefully ask for), and `m.login.sso` besides when
//! a single-sign-on provider is configured ([`crate::config::AuthConfig::sso_available`]).
//! Synapse additionally offers a short grace period after a recent login where UIA is skipped
//! entirely; this server always requires it — a deliberate simplification, noted in
//! `docs/rfcs/0002-auth-tokens-and-requester.md` section 6.
//!
//! # Whose credentials, and for what
//!
//! Two checks bind a session to the request it authorizes, both Synapse's
//! (`AuthHandler.validate_user_via_ui_auth` and `check_ui_auth`):
//!
//! - **The user.** A password stage names its account (`identifier`); the password is checked
//!   against *that* account, and the account is recorded on the session
//!   ([`crate::uia::AUTHENTICATED_USER_KEY`], which the SSO stage records too). Once the flow
//!   completes, an account other than the requester's is `403 M_FORBIDDEN`: a stolen access
//!   token plus the thief's own password must not delete the victim's devices (Sytest's
//!   "DELETE /device/{deviceId} requires UI auth user to match device owner", Complement's
//!   `TestDeviceManagement`). This was missed before because every Sytest account has the same
//!   password, and the check ignored the identifier and tried the requester's own hash.
//! - **The operation** ([`run_for_operation`]): a session is bound to the request that started
//!   it ([`crate::uia::bind_operation`]).

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

fn flows(state: &AuthState, has_password: bool) -> Vec<AuthFlow> {
    let mut flows = if has_password {
        vec![AuthFlow::new(vec![AuthType::Password])]
    } else {
        vec![AuthFlow::new(vec![AuthType::Dummy])]
    };
    if state.config.get().sso_available() {
        flows.push(AuthFlow::new(vec![AuthType::Sso]));
    }
    flows
}

/// Whether the submitted stage passed and, for a stage that authenticates somebody, who.
struct StageResult {
    ok: bool,
    user: Option<ruma::OwnedUserId>,
}

async fn verify(
    state: &AuthState,
    requester: &Requester,
    data: &AuthData,
) -> Result<StageResult, MatrixError> {
    match data {
        AuthData::Dummy(_) => Ok(StageResult {
            ok: true,
            user: None,
        }),
        AuthData::Password(p) => {
            // The account the client named. An identifier this server cannot resolve (an
            // address nobody has) is a failed attempt, like a wrong password.
            let Ok(user_id) =
                crate::routes::login::identifier_to_user_id(state, &p.identifier).await
            else {
                return Ok(StageResult {
                    ok: false,
                    user: None,
                });
            };
            let user = state.store.get_user(&user_id).await?;
            let ok = match user.and_then(|u| u.password_hash) {
                Some(hash) => {
                    password::verify_password(&p.password, &hash, &state.config.get().bcrypt_pepper)
                        .unwrap_or(false)
                }
                None => false,
            };
            if ok && user_id != requester.user_id {
                tracing::info!(
                    requester = %requester.user_id,
                    authenticated = %user_id,
                    "user-interactive auth answered with another account's password"
                );
            }
            Ok(StageResult {
                ok,
                user: ok.then_some(user_id),
            })
        }
        _ => Ok(StageResult {
            ok: false,
            user: None,
        }),
    }
}

/// Rewrites a password stage that names its account the old way (`auth.user`, before
/// identifiers) into `auth.identifier`, which is the only shape `ruma`'s `AuthData` parses.
/// Synapse still reads `user`.
fn normalise_auth(mut auth: Value) -> Value {
    if auth.get("type").and_then(Value::as_str) == Some("m.login.password")
        && auth.get("identifier").is_none()
        && let Some(user) = auth.get("user").cloned()
        && let Some(object) = auth.as_object_mut()
    {
        object.insert(
            "identifier".to_owned(),
            serde_json::json!({"type": "m.id.user", "user": user}),
        );
    }
    auth
}

/// Runs one round of the re-auth flow against `body`'s `auth` field. Returns `Ok(None)` once the
/// flow is complete (the caller should proceed with its sensitive operation) or
/// `Ok(Some(response))` — a `401` with the challenge body — when another round is needed.
///
/// The session is not bound to an operation: callers that can name theirs use
/// [`run_for_operation`]; this entry point is kept for those outside this crate that cannot yet.
///
/// # Errors
/// `403 M_FORBIDDEN` once complete if a stage authenticated another account than the
/// requester's; `400` on a malformed `auth` or an unknown session.
pub async fn run(
    state: &AuthState,
    requester: &Requester,
    body: &Value,
) -> Result<Option<Response>, MatrixError> {
    run_inner(state, requester, body, None).await
}

/// [`run`], with the session bound to `operation` (`"DELETE /devices/ABC"`,
/// `"POST /account/deactivate"`): a session started for one operation and continued on another
/// is `403 M_FORBIDDEN` ([`uia::bind_operation`]).
///
/// # Errors
/// As [`run`], and `403` for an operation that changed.
pub async fn run_for_operation(
    state: &AuthState,
    requester: &Requester,
    body: &Value,
    operation: &str,
) -> Result<Option<Response>, MatrixError> {
    run_inner(state, requester, body, Some(operation)).await
}

async fn run_inner(
    state: &AuthState,
    requester: &Requester,
    body: &Value,
    operation: Option<&str>,
) -> Result<Option<Response>, MatrixError> {
    let user = state
        .store
        .get_user(&requester.user_id)
        .await?
        .ok_or_else(MatrixError::internal)?;
    let flows = flows(state, user.password_hash.is_some());

    let auth: Option<AuthData> = match body.get("auth") {
        Some(v) if !v.is_null() => Some(
            serde_json::from_value(normalise_auth(v.clone()))
                .map_err(|_| MatrixError::invalid_param("invalid auth data"))?,
        ),
        _ => None,
    };
    let timeout = state.config.get().uia_session_timeout_ms;
    let session_id = uia::session_id_for(
        state.store.as_ref(),
        auth.as_ref().and_then(AuthData::session),
        state.now_ms(),
        timeout,
    )
    .await?;
    if let Some(operation) = operation {
        uia::bind_operation(state.store.as_ref(), &session_id, operation).await?;
    }
    let submitted_type = auth.as_ref().and_then(AuthData::auth_type);
    let stage = match &auth {
        Some(data) if submitted_type.is_some() => verify(state, requester, data).await?,
        _ => StageResult {
            ok: true,
            user: None,
        },
    };

    let outcome = uia::advance(
        state.store.as_ref(),
        &flows,
        Some(&session_id),
        submitted_type,
        stage.ok,
        state.now_ms(),
        timeout,
    )
    .await?;
    if let Some(user_id) = &stage.user {
        uia::record_authenticated_user(state.store.as_ref(), &session_id, user_id).await?;
    }

    if outcome.complete {
        if let Some(authenticated) =
            uia::authenticated_user(state.store.as_ref(), &session_id).await?
            && authenticated != requester.user_id.as_str()
        {
            return Err(MatrixError::forbidden(
                "The user-interactive auth was completed as another user",
            ));
        }
        Ok(None)
    } else {
        let body = uia::incomplete_body(flows, outcome.completed, outcome.session_id);
        Ok(Some((StatusCode::UNAUTHORIZED, Json(body)).into_response()))
    }
}
