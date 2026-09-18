//! `GET`/`POST /login`: legacy password, short-term-token and application-service login.
//!
//! Wire shapes for the login mechanism (`LoginInfo`, `UserIdentifier` and their variants) are
//! `ruma::api::client::{session::login::v3, uiaa}` types reused as plain `serde` types (see the
//! module doc on [`crate::uia`] for why that is safe to do without pulling in ruma's endpoint
//! machinery). Everything else about the request (`device_id`, `initial_device_display_name`,
//! `refresh_token`) is read directly off the parsed JSON body.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::response::{IntoResponse, Response};
use axum::{Json, http::StatusCode};
use ruma::api::client::session::login::v3::LoginInfo;
use ruma::api::client::uiaa::UserIdentifier;
use ruma::{OwnedDeviceId, OwnedUserId, UserId};
use serde_json::{Value, json};

use crate::appservice::AppserviceRecord;
use crate::error::{ErrCode, MatrixError};
use crate::password;
use crate::session::{self, NewSession};
use crate::shared_secret_auth;
use crate::state::AuthState;
use crate::token::TokenHash;

/// Synapse's exact wording for "no such user" and "wrong password" alike, deliberately identical
/// so a failed login never tells an attacker which half was wrong.
const INVALID_USERNAME_OR_PASSWORD: &str = "Invalid username or password";

/// The wire login type for `crate::shared_secret_auth` (legacy mautrix bridge double puppeting).
const SHARED_SECRET_AUTH_LOGIN_TYPE: &str = "com.devture.shared_secret_auth";

/// `GET /login`: the login flows this server offers. `m.login.application_service` is
/// deliberately not advertised here — appservices know to use it implicitly from their
/// registration, the same way Synapse omits it from `get_login_types`.
/// `com.devture.shared_secret_auth` is advertised only when a secret is configured for it
/// (`crate::shared_secret_auth`), matching how mautrix bridges probe for it
/// (`refs/mautrix-python/mautrix/bridge/custom_puppet.py`'s
/// `flows.get_first_of_type(LoginType.DEVTURE_SHARED_SECRET, LoginType.PASSWORD)`, read for
/// behavior only): a bridge falls back to `m.login.password` when the flow isn't offered, so a
/// server that never enabled the feature should not pretend to.
pub async fn get_login_types(State(state): State<AuthState>) -> Json<Value> {
    let mut flows = vec![
        json!({"type": "m.login.password"}),
        json!({"type": "m.login.token"}),
    ];
    if state.config.shared_secret_auth_secret.is_some() {
        flows.push(json!({"type": SHARED_SECRET_AUTH_LOGIN_TYPE}));
    }
    Json(json!({ "flows": flows }))
}

/// `POST /login`.
pub async fn post_login(
    State(state): State<AuthState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Result<Response, MatrixError> {
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

    let login_info: LoginInfo = serde_json::from_value(body).map_err(|_| {
        MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::BadJson,
            "Invalid or missing login type",
        )
    })?;

    let user_id = if login_info.login_type() == SHARED_SECRET_AUTH_LOGIN_TYPE {
        resolve_shared_secret_auth_login(&state, &login_info.data()).await?
    } else {
        match login_info {
            LoginInfo::Password(p) => resolve_password_login(&state, &p).await?,
            LoginInfo::Token(t) => resolve_token_login(&state, &t.token).await?,
            LoginInfo::ApplicationService(as_info) => {
                resolve_appservice_login(&state, &headers, &query, as_info.identifier.as_ref())
                    .await?
            }
            _ => {
                return Err(MatrixError::new(
                    StatusCode::BAD_REQUEST,
                    ErrCode::Unrecognized,
                    "Unsupported login type",
                ));
            }
        }
    };

    let user = state
        .store
        .get_user(&user_id)
        .await?
        .ok_or_else(|| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))?;
    if user.deactivated {
        return Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD));
    }
    if user.locked {
        return Err(MatrixError::user_locked());
    }

    let session = session::create_session(
        &state,
        &user_id,
        device_id,
        initial_device_display_name,
        refresh,
    )
    .await?;

    Ok(login_response(&user_id, session).into_response())
}

async fn resolve_password_login(
    state: &AuthState,
    p: &ruma::api::client::session::login::v3::Password,
) -> Result<OwnedUserId, MatrixError> {
    #[allow(deprecated)]
    let user_id = if let Some(identifier) = &p.identifier {
        identifier_to_user_id(state, identifier).await?
    } else if let Some(user) = &p.user {
        UserId::parse_with_server_name(user.as_str(), state.server_name())
            .map_err(|_| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))?
    } else {
        return Err(MatrixError::missing_param("Missing user identifier"));
    };

    let record = state.store.get_user(&user_id).await?;
    let Some(record) = record else {
        return Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD));
    };
    let Some(hash) = &record.password_hash else {
        return Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD));
    };
    let ok = password::verify_password(&p.password, hash, &state.config.bcrypt_pepper)
        .map_err(|_| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))?;
    if !ok {
        return Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD));
    }
    Ok(user_id)
}

/// Resolves a `UserIdentifier` to a full user ID. `m.id.user` parses/validates directly;
/// `m.id.thirdparty` (email) and `m.id.phone` (MSISDN, or a country+number pair) go through
/// [`crate::store::UserStore::get_user_by_threepid`]. The phone-number variant's `country` +
/// `phone` are concatenated digits-only rather than canonicalized through a full E.164
/// country-calling-code table — a day-one simplification noted in
/// `docs/rfcs/0002-auth-tokens-and-requester.md` section 8; clients that already send a bare
/// MSISDN as `phone` with `country` empty are unaffected.
async fn identifier_to_user_id(
    state: &AuthState,
    identifier: &UserIdentifier,
) -> Result<OwnedUserId, MatrixError> {
    match identifier {
        UserIdentifier::Matrix(m) => {
            UserId::parse_with_server_name(m.user.as_str(), state.server_name())
                .map_err(|_| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))
        }
        UserIdentifier::Email(e) => threepid_user_id(state, "email", &e.address).await,
        UserIdentifier::Msisdn(m) => threepid_user_id(state, "msisdn", &m.number).await,
        UserIdentifier::PhoneNumber(p) => {
            let digits: String = p
                .country
                .chars()
                .chain(p.phone.chars())
                .filter(char::is_ascii_digit)
                .collect();
            threepid_user_id(state, "msisdn", &digits).await
        }
        _ => Err(MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD)),
    }
}

async fn threepid_user_id(
    state: &AuthState,
    medium: &str,
    address: &str,
) -> Result<OwnedUserId, MatrixError> {
    state
        .store
        .get_user_by_threepid(medium, address)
        .await?
        .ok_or_else(|| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))
}

async fn resolve_token_login(state: &AuthState, token: &str) -> Result<OwnedUserId, MatrixError> {
    let hash = TokenHash::of(token);
    let record = state
        .store
        .consume_login_token(&hash, state.now_ms())
        .await?;
    record
        .map(|r| r.user_id)
        .ok_or_else(|| MatrixError::forbidden("Invalid login token"))
}

/// Resolves a `com.devture.shared_secret_auth` login (`crate::shared_secret_auth`). `data` is
/// `LoginInfo::data()`'s view of the request body with `type` removed: `{"identifier": {"type":
/// "m.id.user", "user": "..."}, "token": "<hex hmac-sha512>"}`. Returns
/// [`MatrixError::forbidden`] with the same generic wording as a failed password login on any
/// failure (unknown/malformed token, wrong secret, non-`m.id.user` identifier), so a probing
/// client cannot distinguish "this feature is disabled" from "you got the token wrong" — the only
/// case advertised differently is the feature being entirely unconfigured, which
/// [`get_login_types`] simply does not list.
async fn resolve_shared_secret_auth_login(
    state: &AuthState,
    data: &serde_json::Map<String, Value>,
) -> Result<OwnedUserId, MatrixError> {
    let Some(secret) = state.config.shared_secret_auth_secret.as_deref() else {
        return Err(MatrixError::new(
            StatusCode::BAD_REQUEST,
            ErrCode::Unrecognized,
            "com.devture.shared_secret_auth is not enabled on this server",
        ));
    };

    let identifier: UserIdentifier = data
        .get("identifier")
        .cloned()
        .ok_or_else(|| MatrixError::missing_param("Missing identifier"))
        .and_then(|v| {
            serde_json::from_value(v).map_err(|_| MatrixError::invalid_param("invalid identifier"))
        })?;
    let UserIdentifier::Matrix(m) = identifier else {
        return Err(MatrixError::invalid_param(
            "com.devture.shared_secret_auth only supports m.id.user identifiers",
        ));
    };
    let user_id = UserId::parse_with_server_name(m.user.as_str(), state.server_name())
        .map_err(|_| MatrixError::invalid_param("invalid user identifier"))?;

    let token = data
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| MatrixError::missing_param("Missing token"))?;

    shared_secret_auth::verify_token(secret.as_bytes(), &user_id, token)
        .map_err(|_| MatrixError::forbidden(INVALID_USERNAME_OR_PASSWORD))?;

    Ok(user_id)
}

async fn resolve_appservice_login(
    state: &AuthState,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    identifier: Option<&UserIdentifier>,
) -> Result<OwnedUserId, MatrixError> {
    let token = bearer_or_query_token(headers, query).ok_or_else(MatrixError::missing_token)?;
    let record: AppserviceRecord = state
        .appservices
        .lookup_by_token(&token)
        .await
        .ok_or_else(|| MatrixError::unknown_token(false))?;

    let Some(identifier) = identifier else {
        return Ok(record.sender);
    };
    let UserIdentifier::Matrix(m) = identifier else {
        return Err(MatrixError::invalid_param(
            "Application service login only supports m.id.user identifiers",
        ));
    };
    let user_id = UserId::parse_with_server_name(m.user.as_str(), state.server_name())
        .map_err(|_| MatrixError::invalid_param("invalid user identifier"))?;
    if !record.can_control(&user_id) {
        return Err(MatrixError::forbidden(
            "Application service cannot masquerade as this user",
        ));
    }
    // Appservice-driven logins auto-provision the user if it does not exist yet, matching
    // Synapse's behavior for `m.login.application_service` (the AS namespace already proved the
    // right to this user ID; there is no separate registration step for it).
    if state.store.get_user(&user_id).await?.is_none() {
        state
            .store
            .create_user(crate::store::UserRecord::new(
                user_id.clone(),
                state.now_ms(),
            ))
            .await?;
    }
    Ok(user_id)
}

fn bearer_or_query_token(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION)
        && let Ok(raw) = value.to_str()
        && let Some(token) = raw.strip_prefix("Bearer ")
    {
        return Some(token.to_string());
    }
    query.get("access_token").cloned()
}

fn login_response(user_id: &UserId, session: NewSession) -> Json<Value> {
    let mut body = json!({
        "user_id": user_id,
        "access_token": session.access_token,
        "device_id": session.device_id,
    });
    if let Some(rt) = session.refresh_token {
        body["refresh_token"] = json!(rt);
    }
    if let Some(ms) = session.expires_in_ms {
        body["expires_in_ms"] = json!(ms);
    }
    Json(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::password::hash_password;
    use crate::store::{LoginTokenRecord, UserRecord};
    use ruma::user_id;

    async fn state_with_password_user(user_id: &ruma::UserId, password: &str) -> AuthState {
        let state = AuthState::in_memory();
        let mut record = UserRecord::new(user_id.to_owned(), 0);
        record.password_hash = Some(hash_password(password).unwrap());
        state.store.create_user(record).await.unwrap();
        state
    }

    #[tokio::test]
    async fn password_login_by_localpart_succeeds() {
        let state = state_with_password_user(user_id!("@alice:example.org"), "hunter2").await;
        let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "hunter2"});
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn password_login_with_deprecated_user_field_succeeds() {
        let state = state_with_password_user(user_id!("@bob:example.org"), "hunter2").await;
        let body = json!({"type": "m.login.password", "user": "bob", "password": "hunter2"});
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_password_is_forbidden_with_generic_message() {
        let state = state_with_password_user(user_id!("@carol:example.org"), "hunter2").await;
        let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "carol"}, "password": "wrong"});
        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.errcode().as_str(), "M_FORBIDDEN");
    }

    #[tokio::test]
    async fn unknown_user_gives_the_same_generic_message_as_wrong_password() {
        let state = AuthState::in_memory();
        let body = json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "nobody"}, "password": "x"});
        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn password_login_by_email_identifier_succeeds() {
        let state = state_with_password_user(user_id!("@dave:example.org"), "hunter2").await;
        state
            .store
            .bind_threepid(user_id!("@dave:example.org"), "email", "dave@example.org")
            .await
            .unwrap();
        let body = json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.thirdparty", "medium": "email", "address": "dave@example.org"},
            "password": "hunter2"
        });
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn login_token_is_consumed_once() {
        let state = AuthState::in_memory();
        let uid = user_id!("@erin:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let raw_token = "syl_testtoken";
        state
            .store
            .put_login_token(LoginTokenRecord {
                hash: TokenHash::of(raw_token),
                user_id: uid,
                expires_at_ms: state.now_ms() + 60_000,
                used: false,
            })
            .await
            .unwrap();

        let body = json!({"type": "m.login.token", "token": raw_token});
        let response = post_login(
            State(state.clone()),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body.clone()),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn get_login_types_lists_password_and_token() {
        let Json(body) = get_login_types(State(AuthState::in_memory())).await;
        let types: Vec<String> = body["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["type"].as_str().unwrap().to_string())
            .collect();
        assert!(types.contains(&"m.login.password".to_string()));
        assert!(types.contains(&"m.login.token".to_string()));
    }

    #[tokio::test]
    async fn shared_secret_auth_is_not_advertised_when_unconfigured() {
        let Json(body) = get_login_types(State(AuthState::in_memory())).await;
        let types: Vec<String> = body["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["type"].as_str().unwrap().to_string())
            .collect();
        assert!(!types.contains(&SHARED_SECRET_AUTH_LOGIN_TYPE.to_string()));
    }

    #[tokio::test]
    async fn shared_secret_auth_is_advertised_when_configured() {
        let config = crate::config::AuthConfig {
            shared_secret_auth_secret: Some("sekrit".to_string()),
            ..crate::config::AuthConfig::default()
        };
        let Json(body) = get_login_types(State(AuthState::in_memory_with_config(config))).await;
        let types: Vec<String> = body["flows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["type"].as_str().unwrap().to_string())
            .collect();
        assert!(types.contains(&SHARED_SECRET_AUTH_LOGIN_TYPE.to_string()));
    }

    #[tokio::test]
    async fn shared_secret_auth_login_succeeds_with_a_valid_token() {
        let config = crate::config::AuthConfig {
            shared_secret_auth_secret: Some("sekrit".to_string()),
            ..crate::config::AuthConfig::default()
        };
        let state = AuthState::in_memory_with_config(config);
        let uid = user_id!("@puppet:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let token = shared_secret_auth::compute_token(b"sekrit", &uid);
        let body = json!({
            "type": SHARED_SECRET_AUTH_LOGIN_TYPE,
            "identifier": {"type": "m.id.user", "user": "puppet"},
            "token": token,
        });
        let response = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn shared_secret_auth_login_rejects_a_wrong_token() {
        let config = crate::config::AuthConfig {
            shared_secret_auth_secret: Some("sekrit".to_string()),
            ..crate::config::AuthConfig::default()
        };
        let state = AuthState::in_memory_with_config(config);
        let uid = user_id!("@puppet2:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let body = json!({
            "type": SHARED_SECRET_AUTH_LOGIN_TYPE,
            "identifier": {"type": "m.id.user", "user": "puppet2"},
            "token": "not the right token",
        });
        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn shared_secret_auth_login_fails_cleanly_when_disabled() {
        let state = AuthState::in_memory();
        let uid = user_id!("@puppet3:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let body = json!({
            "type": SHARED_SECRET_AUTH_LOGIN_TYPE,
            "identifier": {"type": "m.id.user", "user": "puppet3"},
            "token": "whatever",
        });
        let err = post_login(
            State(state),
            HeaderMap::new(),
            Query(HashMap::new()),
            Json(body),
        )
        .await
        .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_UNRECOGNIZED");
    }
}
