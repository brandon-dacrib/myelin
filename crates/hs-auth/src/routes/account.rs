//! `POST /account/password`, `POST /account/deactivate`, `GET /account/3pid`, `GET /password_policy`.
//!
//! Both mutating endpoints re-authenticate through a single-stage UIA flow: `m.login.password`
//! (re-enter the current password) when the account has one, or `m.login.dummy` when it does not
//! (SSO-only or appservice-provisioned accounts) — there is no password to re-check in that case,
//! so acknowledgement is all UIA can usefully ask for. Synapse additionally offers a short grace
//! period after a recent login where UIA is skipped entirely; this server always requires it,
//! documented as a deliberate simplification in `docs/rfcs/0002-auth-tokens-and-requester.md`
//! section 6.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::error::MatrixError;
use crate::password;
use crate::reauth;
use crate::requester::Requester;
use crate::state::AuthState;
use hs_http::body::PermissiveJson;

/// `POST /account/password`.
pub async fn post_account_password(
    State(state): State<AuthState>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    requester.require_not_suspended()?;

    let new_password = body
        .get("new_password")
        .and_then(Value::as_str)
        .ok_or_else(|| MatrixError::missing_param("Missing new_password"))?;
    state.config.password_policy.validate(new_password)?;

    if let Some(response) = reauth::run(&state, &requester, &body).await? {
        return Ok(response);
    }

    let hash = password::hash_password(new_password).map_err(|_| MatrixError::internal())?;
    state
        .store
        .set_password_hash(&requester.user_id, Some(hash))
        .await?;

    let logout_devices = body
        .get("logout_devices")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if logout_devices {
        if let Some(current) = requester.access_token_id {
            state
                .store
                .delete_other_access_tokens_for_user(&requester.user_id, &current)
                .await?;
        } else {
            state
                .store
                .delete_all_access_tokens_for_user(&requester.user_id)
                .await?;
        }
        state
            .store
            .delete_all_refresh_tokens_for_user(&requester.user_id)
            .await?;
    }

    Ok(Json(json!({})).into_response())
}

/// `POST /account/deactivate`.
pub async fn post_account_deactivate(
    State(state): State<AuthState>,
    requester: Requester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    requester.require_not_suspended()?;

    if let Some(response) = reauth::run(&state, &requester, &body).await? {
        return Ok(response);
    }

    state
        .store
        .set_deactivated(&requester.user_id, true)
        .await?;
    state
        .store
        .set_password_hash(&requester.user_id, None)
        .await?;
    state
        .store
        .delete_all_access_tokens_for_user(&requester.user_id)
        .await?;
    state
        .store
        .delete_all_refresh_tokens_for_user(&requester.user_id)
        .await?;

    // We do not implement identity-server unbinding (no identity-server client in this crate
    // yet); "no-support" is the spec's documented value for "the server did not attempt it".
    Ok(Json(json!({"id_server_unbind_result": "no-support"})).into_response())
}

/// `GET /account/3pid`: the third-party identifiers (email addresses, phone numbers) this
/// homeserver has associated with the caller's account.
///
/// **This server associates none, so the answer is always an empty list** — and an empty list is
/// the spec-complete answer for an account with no third-party identifiers, not a stub. The
/// response shape is exactly what
/// `refs/matrix-spec/data/api/client-server/administrative_contact.yaml` defines; there is simply
/// nothing to put in it.
///
/// That is a statement about this server, not a guess. Nothing here can create a 3PID
/// association: `POST /account/3pid/add` needs a validated session from `.../requestToken`, which
/// needs a mailer or SMS gateway and a validation-session store, and `POST /account/3pid/bind`
/// needs an identity-server client. None of those exist, which is exactly why
/// `GET /_matrix/client/v3/capabilities` already reports `m.3pid_changes: {"enabled": false}`
/// (`crates/hs-cli/src/capabilities.rs`) — the spec's own way for a server to say it does not do
/// this. `UserStore::bind_threepid` is not a counter-example: it is a login-by-email index
/// (`crate::routes::login`'s `m.id.thirdparty` identifier), keyed `(medium, address)` with the
/// user ID as its only value, reachable from no HTTP route, and it stores neither of the
/// `added_at`/`validated_at` timestamps the spec makes **required** on every entry here. Making
/// this endpoint report real rows means a by-user index over that keyspace *and* a value format
/// that carries both timestamps — worth doing when something can actually add a 3PID, and
/// dishonest before then, because the alternative is inventing timestamps.
///
/// It answers `200` rather than `501` because the question has a true answer. Element's Settings
/// page calls this on open and shows the user a visible error when it fails; "you have no
/// third-party identifiers" is both what a client needs to render that page and what is actually
/// the case.
///
/// The [`Requester`] parameter is the point of this signature even though the body ignores it:
/// extracting it is what enforces the endpoint's `accessTokenBearer` security requirement, so an
/// unauthenticated caller gets `401` instead of a list.
pub async fn get_account_3pid(_requester: Requester) -> Json<Value> {
    Json(json!({"threepids": []}))
}

/// `GET /password_policy`: unauthenticated, so clients can show requirements before registration.
pub async fn get_password_policy(State(state): State<AuthState>) -> Json<Value> {
    Json(state.config.password_policy.to_response_json())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthConfig;
    use crate::store::UserRecord;
    use crate::token::TokenHash;
    use axum::http::StatusCode;
    use ruma::user_id;

    async fn state_with_user(password: &str) -> (AuthState, Requester) {
        let state = AuthState::in_memory();
        let uid = user_id!("@alice:example.org").to_owned();
        let mut record = UserRecord::new(uid.clone(), 0);
        record.password_hash = Some(password::hash_password(password).unwrap());
        state.store.create_user(record).await.unwrap();
        let token_hash = TokenHash::of("syt_current");
        state
            .store
            .put_access_token(crate::store::AccessTokenRecord {
                hash: token_hash,
                user_id: uid.clone(),
                device_id: None,
                expires_at_ms: None,
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .unwrap();
        let requester = Requester {
            access_token_id: Some(token_hash),
            ..Requester::for_user(uid)
        };
        (state, requester)
    }

    #[tokio::test]
    async fn password_change_requires_reauth_first() {
        let (state, requester) = state_with_user("oldpassword1").await;
        let body = json!({"new_password": "newpassword1"});
        let response = post_account_password(State(state), requester, PermissiveJson(body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn password_change_succeeds_with_correct_reauth() {
        let (state, requester) = state_with_user("oldpassword1").await;
        let body = json!({
            "new_password": "newpassword1",
            "auth": {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "oldpassword1"}
        });
        let response = post_account_password(
            State(state.clone()),
            requester.clone(),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let user = state
            .store
            .get_user(&requester.user_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            password::verify_password("newpassword1", user.password_hash.as_ref().unwrap(), "")
                .unwrap()
        );
    }

    #[tokio::test]
    async fn password_change_with_wrong_reauth_password_fails() {
        let (state, requester) = state_with_user("oldpassword1").await;
        let body = json!({
            "new_password": "newpassword1",
            "auth": {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "wrongpassword"}
        });
        let err = post_account_password(State(state), requester, PermissiveJson(body))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn logout_devices_default_revokes_other_sessions() {
        let (state, requester) = state_with_user("oldpassword1").await;
        let other_hash = TokenHash::of("syt_other");
        state
            .store
            .put_access_token(crate::store::AccessTokenRecord {
                hash: other_hash,
                user_id: requester.user_id.clone(),
                device_id: None,
                expires_at_ms: None,
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .unwrap();

        let body = json!({
            "new_password": "newpassword1",
            "auth": {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "oldpassword1"}
        });
        post_account_password(
            State(state.clone()),
            requester.clone(),
            PermissiveJson(body),
        )
        .await
        .unwrap();

        assert!(
            state
                .store
                .get_access_token(&other_hash)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .store
                .get_access_token(&requester.access_token_id.unwrap())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn weak_new_password_is_rejected_before_reauth() {
        let mut config = AuthConfig::default();
        config.password_policy.minimum_length = Some(20);
        let state = AuthState::in_memory_with_config(config);
        let uid = user_id!("@bob:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let requester = Requester::for_user(uid);
        let body = json!({"new_password": "short"});
        let err = post_account_password(State(state), requester, PermissiveJson(body))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_WEAK_PASSWORD");
    }

    #[tokio::test]
    async fn deactivate_clears_password_and_revokes_tokens() {
        let (state, requester) = state_with_user("oldpassword1").await;
        let body = json!({"auth": {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "oldpassword1"}});
        let response = post_account_deactivate(
            State(state.clone()),
            requester.clone(),
            PermissiveJson(body),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let user = state
            .store
            .get_user(&requester.user_id)
            .await
            .unwrap()
            .unwrap();
        assert!(user.deactivated);
        assert!(user.password_hash.is_none());
        assert!(
            state
                .store
                .get_access_token(&requester.access_token_id.unwrap())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn suspended_account_cannot_change_password() {
        let (state, mut requester) = state_with_user("oldpassword1").await;
        requester.suspended = true;
        let body = json!({"new_password": "newpassword1"});
        let err = post_account_password(State(state), requester, PermissiveJson(body))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_USER_SUSPENDED");
    }

    /// Element's Settings page calls `GET /account/3pid` as soon as it opens and renders a
    /// visible error if it fails, which is what a missing route gave it. The list being empty is
    /// the whole answer this server has (see the handler's doc comment); what this pins down is
    /// that the key is present and is an array, since a client reading `threepids.length` off a
    /// missing key is the same broken page by a different route.
    #[tokio::test]
    async fn account_3pid_reports_an_empty_list_for_an_account_with_none() {
        let (_state, requester) = state_with_user("hunter2345").await;
        let Json(body) = get_account_3pid(requester).await;
        assert_eq!(
            body["threepids"],
            json!([]),
            "threepids must be present and an empty array, not absent: {body}"
        );
    }

    #[tokio::test]
    async fn password_policy_endpoint_reports_configured_rules() {
        let mut config = AuthConfig::default();
        config.password_policy.minimum_length = Some(8);
        config.password_policy.require_digit = true;
        let state = AuthState::in_memory_with_config(config);
        let Json(body) = get_password_policy(State(state)).await;
        assert_eq!(body["m.minimum_length"], 8);
        assert_eq!(body["m.require_digit"], true);
    }
}
