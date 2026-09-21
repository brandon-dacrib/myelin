//! `POST /register` and `GET /register/available`.
//!
//! Registration drives the same [`crate::uia`] state machine as password change and device
//! deletion. Per the current spec, the client resends the *entire* request body (not just
//! `auth`/`session`) on every round of a multi-stage flow, so this handler does not need to
//! remember `username`/`password` across rounds in UIA session data — it just re-validates them
//! every time and only creates the account once the flow completes.
//!
//! Supported UIA stages: `m.login.dummy` (always available as the fallback flow when nothing else
//! is configured), `m.login.registration_token` (checked against
//! [`crate::config::AuthConfig::valid_registration_tokens`]), `m.login.terms` (acknowledgement
//! only — there is no server-side wording to validate against). `m.login.recaptcha`,
//! `m.login.email.identity` and `m.login.msisdn` are never included in the offered flows (no
//! verification backend exists yet) and fail cleanly with a clear `M_UNRECOGNIZED` if a client
//! submits one anyway, rather than the generic "invalid auth" a genuinely-wrong stage result
//! would get.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rand::Rng;
use rand::distr::Alphanumeric;
use ruma::api::client::uiaa::{AuthData, AuthFlow, AuthType};
use ruma::{OwnedDeviceId, OwnedUserId, UserId};
use serde_json::{Value, json};

use crate::error::{ErrCode, MatrixError};
use crate::password;
use crate::session;
use crate::state::AuthState;
use crate::store::UserRecord;
use crate::uia;
use hs_http::body::PermissiveJson;

fn registration_flows(state: &AuthState) -> Vec<AuthFlow> {
    let mut required = Vec::new();
    if state.config.registration_requires_token {
        required.push(AuthType::RegistrationToken);
    }
    if state.config.terms_enabled {
        required.push(AuthType::Terms);
    }
    if required.is_empty() {
        vec![AuthFlow::new(vec![AuthType::Dummy])]
    } else {
        vec![AuthFlow::new(required)]
    }
}

/// Whether `auth_type` is a stage this server can ever satisfy. Recaptcha/email/msisdn stages are
/// never in [`registration_flows`], so a client only submits one deliberately (an old client
/// hardcoding a flow, or a probe); this is the clean failure path for that.
fn stage_is_supported(auth_type: &AuthType) -> bool {
    matches!(
        auth_type,
        AuthType::Dummy | AuthType::RegistrationToken | AuthType::Terms
    )
}

async fn verify_stage(state: &AuthState, data: &AuthData) -> bool {
    match data {
        AuthData::Dummy(_) => true,
        AuthData::Terms(_) => true,
        AuthData::RegistrationToken(t) => state.config.valid_registration_tokens.contains(&t.token),
        _ => false,
    }
}

/// `GET /register/available?username=...`.
///
/// Deliberately does **not** lower-case `username` the way [`register_user`] does before
/// checking it. Read `refs/synapse/synapse/rest/client/register.py`'s
/// `UsernameAvailabilityRestServlet.on_GET`: it passes the raw query parameter straight to
/// `check_username` with no `.lower()` call (unlike both of `RegisterRestServlet`'s call sites),
/// so an upper-case query on real Synapse gets `M_INVALID_USERNAME` even though `/register` would
/// happily downcase and accept the same string. That is a genuine asymmetry between the two
/// endpoints, not an oversight this crate is inventing — Complement's own
/// `apidoc_register_test.go` never exercises an upper-case `/register/available` query, so there
/// is no conformance test pulling either way; matching Synapse's actual behavior here rather than
/// guessing was the tie-breaker.
pub async fn get_register_available(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, MatrixError> {
    let username = query
        .get("username")
        .ok_or_else(|| MatrixError::missing_param("Missing username"))?;
    validate_localpart(&state, username)?;
    if !state.store.is_localpart_available(username).await? {
        return Err(MatrixError::user_in_use());
    }
    Ok(Json(json!({"available": true})))
}

/// Rejects any localpart outside the spec's *strict* user ID grammar.
///
/// `UserId::parse_with_server_name`'s own validation (`ruma_identifiers_validation::user_id::
/// validate`, used by `UserId::parse`/`TryFrom<&str>`) only rejects a literal `:` or NUL byte —
/// that is the *historical* grammar, kept permissive so a server can still parse old user IDs
/// already sitting in room state (refs/matrix-spec/content/appendices.md, "Historical User IDs":
/// "clients and servers MUST accept user IDs with localparts consisting of any legal
/// non-surrogate Unicode code points except for `:` and `NUL`"). A server *minting a new* user ID
/// must be stricter: the same file's "User Identifiers" section says a localpart "MUST contain
/// only the characters a-z, 0-9, `.`, `_`, `=`, `-`, `/`, and `+`", and the `/register`
/// `operationId`'s own description adds "the server MUST either map the provided `username` onto
/// a `user_id` in a logical manner, or reject any `username` which does not comply to the
/// grammar with `M_INVALID_USERNAME`" (refs/matrix-spec/data/api/client-server/registration.yaml).
/// `UserId::validate_strict` (`ruma_identifiers_validation::user_id::localpart_is_fully_
/// conforming`) is exactly that stricter check.
///
/// Confirmed against Complement's `refs/complement/tests/csapi/apidoc_register_test.go`:
/// "POST /register rejects usernames with special characters" submits localparts containing
/// `!"\:?\\@[]{}|£é\n'` and expects `400 M_INVALID_USERNAME` for every one of them (before UIA is
/// even attempted — Complement's own comment there: "servers are expected to validate request
/// bodies before handling UIA, so 400 is expected here, not 401"), and "GET /register/available
/// returns M_INVALID_USERNAME for invalid user name" does the same for a bare comma. Before this
/// fix, `validate_localpart` accepted all of the above (none of them are `:` or NUL), so
/// `/register/available` reported `available: true` for shapes `/register` would then also
/// silently accept instead of rejecting — the exact gap
/// `docs/status/14-test-and-conformance.md` recorded for this track.
pub(crate) fn validate_localpart(state: &AuthState, username: &str) -> Result<(), MatrixError> {
    let user_id = UserId::parse_with_server_name(username, state.server_name()).map_err(|_| {
        MatrixError::invalid_username(format!("'{username}' is not a valid user ID localpart"))
    })?;
    user_id.validate_strict().map_err(|_| {
        MatrixError::invalid_username(format!("'{username}' is not a valid user ID localpart"))
    })
}

fn random_localpart() -> String {
    std::iter::repeat_with(|| rand::rng().sample(Alphanumeric) as char)
        .filter(char::is_ascii_lowercase)
        .take(12)
        .collect()
}

/// `POST /register?kind=user|guest`.
pub async fn post_register(
    State(state): State<AuthState>,
    Query(query): Query<HashMap<String, String>>,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    let kind = query.get("kind").map(String::as_str).unwrap_or("user");
    if kind == "guest" {
        return register_guest(&state, &body).await;
    }
    if kind != "user" {
        return Err(MatrixError::invalid_param(format!(
            "Unknown registration kind '{kind}'"
        )));
    }
    register_user(&state, &body).await
}

async fn register_guest(state: &AuthState, body: &Value) -> Result<Response, MatrixError> {
    if !state.config.guest_registration_enabled {
        return Err(MatrixError::forbidden("Guest access is disabled"));
    }
    let user_id = fresh_user_id(state).await?;
    state
        .store
        .create_user({
            let mut r = UserRecord::new(user_id.clone(), state.now_ms());
            r.is_guest = true;
            r
        })
        .await?;

    let inhibit_login = body
        .get("inhibit_login")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    finish_registration(state, &user_id, body, inhibit_login).await
}

async fn register_user(state: &AuthState, body: &Value) -> Result<Response, MatrixError> {
    if !state.config.registration_enabled {
        return Err(MatrixError::forbidden("Registration is disabled"));
    }

    let password_raw = body.get("password").and_then(Value::as_str);
    if let Some(pw) = password_raw {
        state.config.password_policy.validate(pw)?;
    }

    // Per the spec's rationale for excluding upper-case from the user ID grammar
    // (appendices.md, "User Identifiers": "we chose to disallow upper-case characters because we
    // do not consider it valid to have two user IDs which differ only in case... [this] requir[es]
    // homeservers to downcase usernames when creating user IDs for new users"), lower-case the
    // client-supplied `username` *before* validating or checking availability, so
    // "User-UPPER" and "user-upper" register the same account instead of two. Matches Synapse's
    // `RegisterRestServlet.on_POST` (`refs/synapse/synapse/rest/client/register.py`:
    // `desired_username = desired_username.lower()`, applied before `check_username`), and
    // Complement's `apidoc_register_test.go` "POST /register downcases capitals in usernames"
    // (registers `user-UPPER`, expects `user_id: "@user-upper:hs1"`). ASCII-only: the grammar
    // itself is ASCII (`validate_localpart` rejects anything else), so a locale-aware
    // `str::to_lowercase` would only risk surprising non-ASCII casing rules for no benefit.
    let username = body
        .get("username")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    if let Some(username) = &username {
        validate_localpart(state, username)?;
        if !state.store.is_localpart_available(username).await? {
            return Err(MatrixError::user_in_use());
        }
    }

    let flows = registration_flows(state);
    let auth: Option<AuthData> = match body.get("auth") {
        Some(v) if !v.is_null() => Some(
            serde_json::from_value(v.clone())
                .map_err(|_| MatrixError::invalid_param("invalid auth data"))?,
        ),
        _ => None,
    };

    let session_id_param = auth.as_ref().and_then(AuthData::session);
    let submitted_type = auth.as_ref().and_then(AuthData::auth_type);

    if let Some(auth_type) = &submitted_type
        && !stage_is_supported(auth_type)
    {
        return Err(MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::Unrecognized,
            format!(
                "The '{}' authentication stage is not supported by this server",
                auth_type.as_ref()
            ),
        ));
    }

    let stage_ok = match &auth {
        Some(data) => verify_stage(state, data).await,
        None => true,
    };

    let outcome = uia::advance(
        state.store.as_ref(),
        &flows,
        session_id_param,
        submitted_type,
        stage_ok,
        state.now_ms(),
        state.config.uia_session_timeout_ms,
    )
    .await?;

    if !outcome.complete {
        let body = uia::incomplete_body(flows, outcome.completed, outcome.session_id);
        return Ok((StatusCode::UNAUTHORIZED, Json(body)).into_response());
    }

    // Re-check availability defensively (closes the TOCTOU window between the early check above
    // and account creation, for two concurrent registrations of the same name).
    let user_id = match &username {
        Some(name) => {
            if !state.store.is_localpart_available(name).await? {
                return Err(MatrixError::user_in_use());
            }
            UserId::parse_with_server_name(name, state.server_name()).map_err(|_| {
                MatrixError::invalid_username(format!("'{name}' is not a valid user ID localpart"))
            })?
        }
        None => fresh_user_id(state).await?,
    };

    let password_hash = match password_raw {
        Some(pw) => Some(password::hash_password(pw).map_err(|_| MatrixError::internal())?),
        None => None,
    };

    state
        .store
        .create_user({
            let mut r = UserRecord::new(user_id.clone(), state.now_ms());
            r.password_hash = password_hash;
            r
        })
        .await?;

    let inhibit_login = body
        .get("inhibit_login")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    finish_registration(state, &user_id, body, inhibit_login).await
}

async fn fresh_user_id(state: &AuthState) -> Result<OwnedUserId, MatrixError> {
    for _ in 0..10 {
        let candidate = random_localpart();
        if state.store.is_localpart_available(&candidate).await? {
            return UserId::parse_with_server_name(candidate, state.server_name())
                .map_err(|_| MatrixError::internal());
        }
    }
    Err(MatrixError::internal())
}

async fn finish_registration(
    state: &AuthState,
    user_id: &UserId,
    body: &Value,
    inhibit_login: bool,
) -> Result<Response, MatrixError> {
    if inhibit_login {
        return Ok(Json(json!({"user_id": user_id})).into_response());
    }

    let device_id: Option<OwnedDeviceId> = body
        .get("device_id")
        .and_then(Value::as_str)
        .map(Into::into);
    let initial_device_display_name = body
        .get("initial_device_display_name")
        .and_then(Value::as_str)
        .map(String::from);
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let session = session::create_session(
        state,
        user_id,
        device_id,
        initial_device_display_name,
        refresh,
    )
    .await?;

    let mut response = json!({
        "user_id": user_id,
        "access_token": session.access_token,
        "device_id": session.device_id,
    });
    if let Some(rt) = session.refresh_token {
        response["refresh_token"] = json!(rt);
    }
    if let Some(ms) = session.expires_in_ms {
        response["expires_in_ms"] = json!(ms);
    }
    Ok(Json(response).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthConfig;

    #[tokio::test]
    async fn registration_completes_with_dummy_stage_by_default() {
        let state = AuthState::in_memory();
        let body = json!({"username": "newuser", "password": "hunter22", "auth": {"type": "m.login.dummy"}});
        let response = post_register(State(state), Query(HashMap::new()), PermissiveJson(body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn first_call_without_auth_returns_401_with_flows() {
        let state = AuthState::in_memory();
        let body = json!({"username": "newuser2", "password": "hunter22"});
        let response = post_register(State(state), Query(HashMap::new()), PermissiveJson(body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["session"].is_string());
        assert!(json["flows"].is_array());
    }

    #[tokio::test]
    async fn weak_password_is_rejected_before_uia() {
        let mut config = AuthConfig::default();
        config.password_policy.minimum_length = Some(10);
        let state = AuthState::in_memory_with_config(config);
        let body =
            json!({"username": "shortpw", "password": "short", "auth": {"type": "m.login.dummy"}});
        let err = post_register(State(state), Query(HashMap::new()), PermissiveJson(body))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_WEAK_PASSWORD");
    }

    #[tokio::test]
    async fn duplicate_username_is_rejected() {
        let state = AuthState::in_memory();
        let body =
            json!({"username": "dupe", "password": "hunter22", "auth": {"type": "m.login.dummy"}});
        let response = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            PermissiveJson(body.clone()),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let err = post_register(State(state), Query(HashMap::new()), PermissiveJson(body))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_USER_IN_USE");
    }

    #[tokio::test]
    async fn registration_token_stage_requires_a_valid_token() {
        let mut config = AuthConfig {
            registration_requires_token: true,
            ..AuthConfig::default()
        };
        config
            .valid_registration_tokens
            .insert("good-token".to_string());
        let state = AuthState::in_memory_with_config(config);

        let body = json!({"username": "tokenuser", "auth": {"type": "m.login.registration_token", "token": "bad-token"}});
        let err = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            PermissiveJson(body),
        )
        .await
        .unwrap_err();
        // Refused, and free to try again with a token that works: see `uia::advance`.
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(err.errcode(), crate::error::ErrCode::Forbidden);

        let body = json!({"username": "tokenuser", "auth": {"type": "m.login.registration_token", "token": "good-token"}});
        let response = post_register(State(state), Query(HashMap::new()), PermissiveJson(body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn recaptcha_stage_fails_cleanly_when_submitted() {
        let state = AuthState::in_memory();
        let body = json!({"username": "recaptchauser", "auth": {"type": "m.login.recaptcha", "response": "x"}});
        let err = post_register(State(state), Query(HashMap::new()), PermissiveJson(body))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_UNRECOGNIZED");
    }

    #[tokio::test]
    async fn inhibit_login_skips_token_issuance() {
        let state = AuthState::in_memory();
        let body = json!({
            "username": "noauto",
            "password": "hunter22",
            "inhibit_login": true,
            "auth": {"type": "m.login.dummy"}
        });
        let response = post_register(State(state), Query(HashMap::new()), PermissiveJson(body))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("access_token").is_none());
        assert_eq!(json["user_id"], "@noauto:example.org");
    }

    #[tokio::test]
    async fn guest_registration_respects_config_flag() {
        let state = AuthState::in_memory(); // guest_registration_enabled: false by default
        let mut query = HashMap::new();
        query.insert("kind".to_string(), "guest".to_string());
        let err = post_register(State(state), Query(query), PermissiveJson(json!({})))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn guest_registration_succeeds_when_enabled() {
        let config = AuthConfig {
            guest_registration_enabled: true,
            ..AuthConfig::default()
        };
        let state = AuthState::in_memory_with_config(config);
        let mut query = HashMap::new();
        query.insert("kind".to_string(), "guest".to_string());
        let response = post_register(State(state), Query(query), PermissiveJson(json!({})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn register_available_reports_taken_and_free_names() {
        let state = AuthState::in_memory();
        let body =
            json!({"username": "taken", "password": "hunter22", "auth": {"type": "m.login.dummy"}});
        post_register(
            State(state.clone()),
            Query(HashMap::new()),
            PermissiveJson(body),
        )
        .await
        .unwrap();

        let mut query = HashMap::new();
        query.insert("username".to_string(), "taken".to_string());
        let err = get_register_available(State(state.clone()), Query(query))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_USER_IN_USE");

        let mut query = HashMap::new();
        query.insert("username".to_string(), "free".to_string());
        let Json(body) = get_register_available(State(state), Query(query))
            .await
            .unwrap();
        assert_eq!(body["available"], true);
    }

    /// `refs/complement/tests/csapi/apidoc_register_test.go`: "GET /register/available returns
    /// M_INVALID_USERNAME for invalid user name" uses `"username,should_not_be_valid"` (a comma is
    /// legal ASCII, not `:` or NUL, so the old lenient check accepted it and reported
    /// `available: true`; want `400 M_INVALID_USERNAME`).
    #[tokio::test]
    async fn register_available_rejects_an_invalid_username_shape() {
        let state = AuthState::in_memory();
        let mut query = HashMap::new();
        query.insert(
            "username".to_string(),
            "username,should_not_be_valid".to_string(),
        );
        let err = get_register_available(State(state), Query(query))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.errcode().as_str(), "M_INVALID_USERNAME");
    }

    /// `refs/complement/tests/csapi/apidoc_register_test.go`: "POST /register rejects usernames
    /// with special characters" — every one of these localparts must 400 with `M_INVALID_USERNAME`
    /// *before* UIA runs (Complement's own comment: "servers are expected to validate request
    /// bodies before handling UIA, so 400 is expected here, not 401"). None of these characters are
    /// `:` or NUL, so the old `UserId::parse_with_server_name`-only check accepted all of them.
    #[tokio::test]
    async fn register_rejects_usernames_with_special_characters() {
        let state = AuthState::in_memory();
        for ch in [
            "!", "\"", ":", "?", "\\", "@", "[", "]", "{", "|", "}", "£", "é", "\n", "'",
        ] {
            let body = json!({
                "username": format!("user-{ch}-reject-please"),
                "password": "sUp3rs3kr1t",
            });
            let err = post_register(
                State(state.clone()),
                Query(HashMap::new()),
                PermissiveJson(body),
            )
            .await
            .unwrap_err();
            assert_eq!(err.status(), StatusCode::BAD_REQUEST, "char {ch:?}");
            assert_eq!(err.errcode().as_str(), "M_INVALID_USERNAME", "char {ch:?}");
        }
    }

    /// `refs/complement/tests/csapi/apidoc_register_test.go`: "POST /register downcases capitals
    /// in usernames" — registering `user-UPPER` must succeed with `user_id: "@user-upper:..."`,
    /// not store the localpart verbatim (which would let `@user-UPPER:...` and a later
    /// `@user-upper:...` registration coexist as two accounts a client can't tell apart, per the
    /// spec's user-ID-grammar rationale).
    #[tokio::test]
    async fn register_downcases_uppercase_usernames() {
        let state = AuthState::in_memory();
        let body = json!({
            "username": "user-UPPER",
            "password": "sUp3rs3kr1t",
            "auth": {"type": "m.login.dummy"}
        });
        let response = post_register(State(state), Query(HashMap::new()), PermissiveJson(body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["user_id"], "@user-upper:example.org");
    }

    /// The reverse direction of the above: once `user-upper` is lower-cased and stored, a second
    /// registration attempt spelled with different capitalization must collide with it rather than
    /// creating a second account — the exact "two accounts a client considers the same" failure
    /// mode this fix closes.
    #[tokio::test]
    async fn register_treats_different_capitalizations_as_the_same_username() {
        let state = AuthState::in_memory();
        let first = json!({
            "username": "CaseCollide",
            "password": "sUp3rs3kr1t",
            "auth": {"type": "m.login.dummy"}
        });
        let response = post_register(
            State(state.clone()),
            Query(HashMap::new()),
            PermissiveJson(first),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let second = json!({
            "username": "casecollide",
            "password": "sUp3rs3kr1t",
            "auth": {"type": "m.login.dummy"}
        });
        let err = post_register(State(state), Query(HashMap::new()), PermissiveJson(second))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_USER_IN_USE");
    }

    /// Registration in a single round trip -- `username`/`password`/`auth: {"type":
    /// "m.login.dummy"}` sent all at once, no prior call to fetch the session -- must keep
    /// working. Complement's `apidoc_register_test.go` "Registration without a session fails"
    /// wants a stricter rule that would break exactly this pattern; `refs/synapse/synapse/
    /// handlers/auth.py::AuthHandler.check_ui_auth` confirms real Synapse allows it (mints a
    /// fresh session and completes the stage on it in the same call whenever `session` is
    /// absent), and that Complement test itself skips Synapse, Dendrite and Conduit for the same
    /// reason. See `uia::advance`'s doc comment and `docs/status/07-auth-and-identity.md` for the
    /// full read.
    #[tokio::test]
    async fn registration_completes_in_a_single_round_trip_with_no_prior_session() {
        let state = AuthState::in_memory();
        let body = json!({
            "username": "single-round-trip",
            "password": "sUp3rs3kr1t",
            "auth": {"type": "m.login.dummy"}
        });
        let response = post_register(State(state), Query(HashMap::new()), PermissiveJson(body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
