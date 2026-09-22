//! The `Requester` extractor: the middleware contract every HTTP handler in the workspace is
//! meant to use (`docs/workstreams/README.md`'s week-6 seam, "Authentication middleware: request
//! to `Requester`"). Add `Requester` (or [`AllowGuest`]) as a handler parameter and axum runs
//! [`FromRequestParts::from_request_parts`] before the handler body, producing either a populated
//! [`Requester`] or a `401`/`403` [`MatrixError`] response — the handler never touches headers or
//! query parameters for authentication itself.
//!
//! Precedence and error codes reproduce Synapse's `synapse/api/auth/base.py` and
//! `synapse/api/auth/internal.py` (`get_access_token_from_request`, `get_appservice_user`,
//! `_wrapped_get_user_by_req`; behavioral reference only, no code copied):
//!
//! 1. Exactly one of an `Authorization: Bearer <token>` header or an `access_token` query
//!    parameter must be present; both together is `401 M_MISSING_TOKEN` ("mixing"), as is
//!    neither.
//! 2. The token is checked against the appservice registry first. A match authenticates as the
//!    appservice, honoring `user_id` (masquerade, validated against the appservice's namespaces)
//!    and `device_id` / `org.matrix.msc3202.device_id` (masquerade, validated to be a real device
//!    of the effective user). The unstable-prefixed parameter name is accepted in addition to the
//!    stabilized `device_id`, since some appservices in the wild still send the former.
//! 3. Otherwise the token is looked up as an ordinary access token. A missing or expired token is
//!    `401 M_UNKNOWN_TOKEN` (expired sets `soft_logout: true`, telling the client it is safe to
//!    keep local room state while it re-authenticates). A locked account is `401 M_USER_LOCKED`.
//!    A deactivated account's tokens are treated as unknown (deactivation revokes tokens; this
//!    branch only fires for a token that somehow outlived that).

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use ruma::OwnedDeviceId;
use std::collections::HashMap;

use crate::error::MatrixError;
use crate::requester::{AppserviceIdentity, Requester};
use crate::state::AuthState;
use crate::token::TokenHash;

/// Reads `access_token` and the appservice masquerade parameters from the query string. Uses
/// axum's own `Query` extractor (already a transitive dependency of `axum`) rather than adding a
/// new URL-decoding dependency; an unparseable query string degrades to "no parameters" rather
/// than a hard error, since a broken query string should surface as "missing token", not as a
/// 400 that leaks parser internals.
pub(crate) async fn query_params(parts: &mut Parts, state: &AuthState) -> HashMap<String, String> {
    axum::extract::Query::<HashMap<String, String>>::from_request_parts(parts, state)
        .await
        .map(|q| q.0)
        .unwrap_or_default()
}

/// The bearer token in `headers`, if there is one. `Ok(None)` for no `Authorization` header at
/// all; an error for a header that is there and wrong.
pub(crate) fn bearer_token(headers: &axum::http::HeaderMap) -> Result<Option<String>, MatrixError> {
    let values: Vec<_> = headers.get_all(AUTHORIZATION).iter().collect();
    if values.is_empty() {
        return Ok(None);
    }
    if values.len() > 1 {
        return Err(MatrixError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            crate::error::ErrCode::MissingToken,
            "Too many Authorization headers.",
        ));
    }
    let raw = values[0].to_str().map_err(|_| {
        MatrixError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            crate::error::ErrCode::MissingToken,
            "Invalid Authorization header.",
        )
    })?;
    match raw.split_once(' ') {
        Some(("Bearer", token)) if !token.is_empty() => Ok(Some(token.to_string())),
        _ => Err(MatrixError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            crate::error::ErrCode::MissingToken,
            "Invalid Authorization header.",
        )),
    }
}

pub(crate) async fn extract_token(
    parts: &mut Parts,
    state: &AuthState,
) -> Result<String, MatrixError> {
    let bearer = bearer_token(&parts.headers)?;
    let query = query_params(parts, state).await;
    let query_token = query.get("access_token").cloned();
    match (bearer, query_token) {
        (Some(_), Some(_)) => Err(MatrixError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            crate::error::ErrCode::MissingToken,
            "Mixing Authorization headers and access_token query parameters.",
        )),
        (Some(token), None) => Ok(token),
        (None, Some(token)) if state.config.accept_legacy_query_param_token => Ok(token),
        _ => Err(MatrixError::missing_token()),
    }
}

async fn authenticate_appservice(
    token: &str,
    query: &HashMap<String, String>,
    state: &AuthState,
) -> Result<Option<Requester>, MatrixError> {
    let Some(record) = state.appservices.lookup_by_token(token).await else {
        return Ok(None);
    };

    let effective_user_id = match query.get("user_id") {
        Some(raw) => {
            let uid = ruma::UserId::parse(raw.as_str())
                .map_err(|_| MatrixError::invalid_param(format!("invalid user_id: {raw}")))?;
            if !record.can_control(&uid) {
                return Err(MatrixError::forbidden(
                    "Application service cannot masquerade as this user",
                ));
            }
            uid
        }
        None => record.sender.clone(),
    };

    let device_param = query
        .get("org.matrix.msc3202.device_id")
        .or_else(|| query.get("device_id"));
    let masqueraded_device_id = match device_param {
        Some(raw) => {
            let device_id: OwnedDeviceId = raw.as_str().into();
            let exists = state
                .store
                .get_device(&effective_user_id, &device_id)
                .await?
                .is_some();
            if !exists {
                return Err(MatrixError::unknown_device(format!(
                    "Application service trying to use a device that doesn't exist ('{raw}' for {effective_user_id})"
                )));
            }
            Some(device_id)
        }
        None => None,
    };

    Ok(Some(Requester {
        user_id: effective_user_id.clone(),
        device_id: masqueraded_device_id.clone(),
        is_guest: false,
        is_admin: false,
        shadow_banned: false,
        suspended: false,
        appservice: Some(AppserviceIdentity {
            appservice_id: record.appservice_id.clone(),
            sender: record.sender.clone(),
            masqueraded_user: effective_user_id != record.sender,
            masqueraded_device_id,
            rate_limited: record.rate_limited,
            msc4190_enabled: record.msc4190_enabled,
        }),
        access_token_id: Some(TokenHash::of(token)),
    }))
}

async fn authenticate_user_token(token: &str, state: &AuthState) -> Result<Requester, MatrixError> {
    let hash = TokenHash::of(token);
    let Some(record) = state.store.get_access_token(&hash).await? else {
        return Err(MatrixError::unknown_token(false));
    };

    let now = state.now_ms();
    if let Some(expires_at) = record.expires_at_ms
        && expires_at < now
    {
        return Err(MatrixError::unknown_token(true));
    }

    let Some(user) = state.store.get_user(&record.user_id).await? else {
        return Err(MatrixError::unknown_token(false));
    };

    if user.deactivated {
        return Err(MatrixError::unknown_token(false));
    }
    if user.locked {
        return Err(MatrixError::user_locked());
    }

    state.store.mark_access_token_used(&hash, now).await?;
    if let Some(device_id) = &record.device_id {
        state
            .store
            .record_seen(&user.user_id, device_id, now, None)
            .await?;
    }

    Ok(Requester {
        user_id: user.user_id.clone(),
        device_id: record.device_id.clone(),
        is_guest: user.is_guest,
        is_admin: user.is_admin,
        shadow_banned: user.shadow_banned,
        suspended: user.suspended,
        appservice: None,
        access_token_id: Some(hash),
    })
}

async fn authenticate(parts: &mut Parts, state: &AuthState) -> Result<Requester, MatrixError> {
    let token = extract_token(parts, state).await?;
    let query = query_params(parts, state).await;

    if let Some(requester) = authenticate_appservice(&token, &query, state).await? {
        return Ok(requester);
    }

    authenticate_user_token(&token, state).await
}

impl FromRequestParts<AuthState> for Requester {
    type Rejection = MatrixError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AuthState,
    ) -> Result<Self, Self::Rejection> {
        let requester = authenticate(parts, state).await?;
        if requester.is_guest {
            return Err(MatrixError::guest_access_forbidden());
        }
        Ok(requester)
    }
}

/// The same authentication as [`Requester`], but does not reject guest accounts. Handlers that
/// the spec says guests may use (`/account/whoami`, `/logout`, ...) take `AllowGuest` instead of
/// `Requester`.
pub struct AllowGuest(pub Requester);

impl FromRequestParts<AuthState> for AllowGuest {
    type Rejection = MatrixError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AuthState,
    ) -> Result<Self, Self::Rejection> {
        Ok(AllowGuest(authenticate(parts, state).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appservice::{AppserviceRecord, NamespaceRule};
    use crate::store::{AccessTokenRecord, DeviceRecord, UserRecord};
    use axum::body::Body;
    use axum::http::Request;
    use ruma::{device_id, user_id};

    async fn parts_for(req: Request<Body>) -> Parts {
        let (parts, _body) = req.into_parts();
        parts
    }

    #[tokio::test]
    async fn missing_token_is_rejected() {
        let state = AuthState::in_memory();
        let mut parts = parts_for(Request::builder().uri("/x").body(Body::empty()).unwrap()).await;
        let err = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(err.errcode().as_str(), "M_MISSING_TOKEN");
    }

    #[tokio::test]
    async fn mixing_header_and_query_param_is_rejected() {
        let state = AuthState::in_memory();
        let mut parts = parts_for(
            Request::builder()
                .uri("/x?access_token=abc")
                .header(AUTHORIZATION, "Bearer abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let err = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_MISSING_TOKEN");
    }

    #[tokio::test]
    async fn unknown_token_is_rejected() {
        let state = AuthState::in_memory();
        let mut parts = parts_for(
            Request::builder()
                .uri("/x")
                .header(AUTHORIZATION, "Bearer syt_nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let err = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_UNKNOWN_TOKEN");
    }

    #[tokio::test]
    async fn valid_bearer_token_authenticates() {
        let state = AuthState::in_memory();
        let uid = user_id!("@alice:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let token = "syt_faketoken";
        let hash = TokenHash::of(token);
        state
            .store
            .put_access_token(AccessTokenRecord {
                hash,
                user_id: uid.clone(),
                device_id: None,
                expires_at_ms: None,
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .unwrap();

        let mut parts = parts_for(
            Request::builder()
                .uri("/x")
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let requester = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        assert_eq!(requester.user_id, uid);
    }

    #[tokio::test]
    async fn legacy_query_param_token_authenticates() {
        let state = AuthState::in_memory();
        let uid = user_id!("@bob:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let token = "syt_query";
        state
            .store
            .put_access_token(AccessTokenRecord {
                hash: TokenHash::of(token),
                user_id: uid.clone(),
                device_id: None,
                expires_at_ms: None,
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .unwrap();

        let mut parts = parts_for(
            Request::builder()
                .uri(format!("/x?access_token={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let requester = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        assert_eq!(requester.user_id, uid);
    }

    #[tokio::test]
    async fn query_param_token_rejected_when_disabled() {
        let config = crate::config::AuthConfig {
            accept_legacy_query_param_token: false,
            ..crate::config::AuthConfig::default()
        };
        let state = AuthState::in_memory_with_config(config);
        let mut parts = parts_for(
            Request::builder()
                .uri("/x?access_token=whatever")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let err = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_MISSING_TOKEN");
    }

    #[tokio::test]
    async fn locked_user_gets_user_locked() {
        let state = AuthState::in_memory();
        let uid = user_id!("@locked:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        state.store.set_locked(&uid, true).await.unwrap();
        let token = "syt_locked";
        state
            .store
            .put_access_token(AccessTokenRecord {
                hash: TokenHash::of(token),
                user_id: uid,
                device_id: None,
                expires_at_ms: None,
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .unwrap();
        let mut parts = parts_for(
            Request::builder()
                .uri("/x")
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let err = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_USER_LOCKED");
    }

    #[tokio::test]
    async fn expired_token_is_soft_logout() {
        let state = AuthState::in_memory();
        let uid = user_id!("@expired:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let token = "syt_expired";
        state
            .store
            .put_access_token(AccessTokenRecord {
                hash: TokenHash::of(token),
                user_id: uid,
                device_id: None,
                expires_at_ms: Some(1), // already in the past relative to the real clock
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .unwrap();
        let mut parts = parts_for(
            Request::builder()
                .uri("/x")
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let err = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_UNKNOWN_TOKEN");
    }

    #[tokio::test]
    async fn guest_is_rejected_by_requester_but_allowed_by_allow_guest() {
        let state = AuthState::in_memory();
        let uid = user_id!("@guest1:example.org").to_owned();
        let mut record = UserRecord::new(uid.clone(), 0);
        record.is_guest = true;
        state.store.create_user(record).await.unwrap();
        let token = "syt_guest";
        state
            .store
            .put_access_token(AccessTokenRecord {
                hash: TokenHash::of(token),
                user_id: uid.clone(),
                device_id: None,
                expires_at_ms: None,
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .unwrap();

        let mut parts = parts_for(
            Request::builder()
                .uri("/x")
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let err = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_GUEST_ACCESS_FORBIDDEN");

        let mut parts2 = parts_for(
            Request::builder()
                .uri("/x")
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let AllowGuest(requester) = AllowGuest::from_request_parts(&mut parts2, &state)
            .await
            .unwrap();
        assert_eq!(requester.user_id, uid);
        assert!(requester.is_guest);
    }

    #[tokio::test]
    async fn appservice_token_authenticates_as_sender_by_default() {
        let state = AuthState::in_memory();
        let sender = user_id!("@bridge:example.org").to_owned();
        let record = AppserviceRecord::new(
            "bridge1",
            sender.clone(),
            vec![NamespaceRule {
                regex: regex::Regex::new(r"^@bridge_.*:example\.org$").unwrap(),
                exclusive: true,
            }],
        );
        let registry = crate::appservice::InMemoryAppserviceRegistry::new();
        registry.insert("as_token", record);
        let state = AuthState {
            appservices: std::sync::Arc::new(registry),
            ..state
        };

        let mut parts = parts_for(
            Request::builder()
                .uri("/x")
                .header(AUTHORIZATION, "Bearer as_token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let requester = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        assert_eq!(requester.user_id, sender);
        assert!(requester.appservice.is_some());
        assert!(!requester.appservice.unwrap().masqueraded_user);
    }

    #[tokio::test]
    async fn appservice_can_masquerade_as_namespaced_user() {
        let state = AuthState::in_memory();
        let sender = user_id!("@bridge:example.org").to_owned();
        let record = AppserviceRecord::new(
            "bridge1",
            sender.clone(),
            vec![NamespaceRule {
                regex: regex::Regex::new(r"^@bridge_.*:example\.org$").unwrap(),
                exclusive: true,
            }],
        );
        let registry = crate::appservice::InMemoryAppserviceRegistry::new();
        registry.insert("as_token", record);
        let state = AuthState {
            appservices: std::sync::Arc::new(registry),
            ..state
        };

        let mut parts = parts_for(
            Request::builder()
                .uri("/x?user_id=@bridge_alice:example.org")
                .header(AUTHORIZATION, "Bearer as_token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let requester = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        assert_eq!(requester.user_id.as_str(), "@bridge_alice:example.org");
        assert!(requester.appservice.unwrap().masqueraded_user);
    }

    #[tokio::test]
    async fn appservice_cannot_masquerade_outside_namespace() {
        let state = AuthState::in_memory();
        let sender = user_id!("@bridge:example.org").to_owned();
        let record = AppserviceRecord::new(
            "bridge1",
            sender.clone(),
            vec![NamespaceRule {
                regex: regex::Regex::new(r"^@bridge_.*:example\.org$").unwrap(),
                exclusive: true,
            }],
        );
        let registry = crate::appservice::InMemoryAppserviceRegistry::new();
        registry.insert("as_token", record);
        let state = AuthState {
            appservices: std::sync::Arc::new(registry),
            ..state
        };

        let mut parts = parts_for(
            Request::builder()
                .uri("/x?user_id=@someone_else:example.org")
                .header(AUTHORIZATION, "Bearer as_token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let err = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_FORBIDDEN");
    }

    #[tokio::test]
    async fn appservice_device_masquerade_via_msc3202_param() {
        let state = AuthState::in_memory();
        let sender = user_id!("@bridge:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(sender.clone(), 0))
            .await
            .unwrap();
        state
            .store
            .upsert_device(DeviceRecord {
                user_id: sender.clone(),
                device_id: device_id!("BOTDEV").to_owned(),
                display_name: None,
                last_seen_ms: None,
                last_seen_ip: None,
            })
            .await
            .unwrap();
        let record = AppserviceRecord::new("bridge1", sender.clone(), vec![]);
        let registry = crate::appservice::InMemoryAppserviceRegistry::new();
        registry.insert("as_token", record);
        let state = AuthState {
            appservices: std::sync::Arc::new(registry),
            ..state
        };

        let mut parts = parts_for(
            Request::builder()
                .uri("/x?org.matrix.msc3202.device_id=BOTDEV")
                .header(AUTHORIZATION, "Bearer as_token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let requester = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        assert_eq!(requester.device_id.as_deref(), Some(device_id!("BOTDEV")));
    }

    #[tokio::test]
    async fn appservice_unknown_device_is_rejected() {
        let state = AuthState::in_memory();
        let sender = user_id!("@bridge:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(sender.clone(), 0))
            .await
            .unwrap();
        let record = AppserviceRecord::new("bridge1", sender.clone(), vec![]);
        let registry = crate::appservice::InMemoryAppserviceRegistry::new();
        registry.insert("as_token", record);
        let state = AuthState {
            appservices: std::sync::Arc::new(registry),
            ..state
        };

        let mut parts = parts_for(
            Request::builder()
                .uri("/x?device_id=NOPE")
                .header(AUTHORIZATION, "Bearer as_token")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let err = Requester::from_request_parts(&mut parts, &state)
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_UNKNOWN_DEVICE");
    }
}
