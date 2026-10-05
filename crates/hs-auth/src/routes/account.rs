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
    state.config.get().password_policy.validate(new_password)?;

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
        // The sessions are gone; so are the pushers they registered (`hs-push`'s observer).
        let kept_device = requester
            .access_token_id
            .as_ref()
            .and(requester.device_id.as_deref());
        state
            .notify_other_sessions_revoked(&requester.user_id, kept_device)
            .await;
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

    // An appservice deactivating one of its users has no password to re-enter and nobody to
    // ask: its token is the authority, as in Synapse (`DeactivateAccountRestServlet`) and
    // Sytest's "AS can deactivate a user". The masquerade was already checked against its
    // namespace before this `Requester` existed.
    if let Some(appservice) = &requester.appservice {
        tracing::info!(user = %requester.user_id, appservice = %appservice.appservice_id, "an appservice deactivated one of its users");
    } else if let Some(response) = reauth::run(&state, &requester, &body).await? {
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

    // The spec's `erase` (MSC2438): the same erasure an administrator's `users.deactivate`
    // with `erase: true` performs, minus leaving the account's rooms, which this crate cannot
    // see (see `crate::erasure`); the devices' keys go with the devices, through the
    // device-list hook.
    if body.get("erase").and_then(Value::as_bool) == Some(true) {
        let erased =
            crate::erasure::erase_account(state.store.as_ref(), &requester.user_id, state.now_ms())
                .await?;
        if erased.devices_deleted > 0 {
            state.notify_device_list_changed(&requester.user_id).await;
        }
        tracing::info!(
            user = %requester.user_id,
            devices_deleted = erased.devices_deleted,
            "account erased at its owner's request"
        );
    }

    // We do not implement identity-server unbinding (no identity-server client in this crate
    // yet); "no-support" is the spec's documented value for "the server did not attempt it".
    Ok(Json(json!({"id_server_unbind_result": "no-support"})).into_response())
}

/// `GET /account/3pid`: the third-party identifiers (email addresses, phone numbers) this
/// homeserver has associated with the caller's account.
///
/// Since 2026-09-28 these are real: an administrator binds them (`users.threepids.add` in the
/// admin API, [`crate::store::IdentityStore::add_threepid`]), and each is listed here with the
/// `added_at`/`validated_at` timestamps the spec makes required (an administrator's binding is
/// validated when it is made). A user still cannot add, bind or remove one themself -- that
/// needs a mailer or SMS gateway and a validation-session store, or an identity-server client,
/// none of which exist -- so `GET /_matrix/client/v3/capabilities` goes on reporting
/// `m.3pid_changes: {"enabled": false}`.
///
/// An account with none gets an empty list, which is the spec-complete answer, not a stub:
/// Element's Settings page calls this on open and shows a visible error when it fails.
pub async fn get_account_3pid(
    State(state): State<AuthState>,
    requester: Requester,
) -> Result<Json<Value>, MatrixError> {
    let threepids = state
        .store
        .list_threepids(&requester.user_id)
        .await
        .map_err(|error| {
            tracing::error!(user_id = %requester.user_id, %error, "listing 3PIDs failed");
            MatrixError::internal()
        })?;
    let threepids: Vec<Value> = threepids
        .into_iter()
        .map(|t| {
            json!({
                "medium": t.medium,
                "address": t.address,
                "added_at": t.added_at_ms,
                "validated_at": t.validated_at_ms,
            })
        })
        .collect();
    Ok(Json(json!({ "threepids": threepids })))
}

/// `GET /password_policy`: unauthenticated, so clients can show requirements before registration.
pub async fn get_password_policy(State(state): State<AuthState>) -> Json<Value> {
    Json(state.config.get().password_policy.to_response_json())
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
        // A failed stage is the 401 challenge again with `M_FORBIDDEN` added, not a bare 403:
        // the client keeps its session and may try the password a second time (`uia::advance`).
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(err.errcode(), crate::error::ErrCode::Forbidden);
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

    /// Sytest's "AS can deactivate a user": an appservice deactivating its ghost is not asked to
    /// re-authenticate (it has no password to give), and the account is deactivated.
    #[tokio::test]
    async fn an_appservice_deactivates_its_user_without_uia() {
        let state = AuthState::in_memory();
        let ghost = user_id!("@irc_bob:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(ghost.clone(), 0))
            .await
            .unwrap();
        let mut requester = Requester::for_user(ghost.clone());
        requester.appservice = Some(crate::requester::AppserviceIdentity {
            appservice_id: "irc".to_owned(),
            sender: user_id!("@ircbot:example.org").to_owned(),
            masqueraded_user: true,
            masqueraded_device_id: None,
            rate_limited: true,
            msc4190_enabled: false,
        });
        let response =
            post_account_deactivate(State(state.clone()), requester, PermissiveJson(json!({})))
                .await
                .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            state
                .store
                .get_user(&ghost)
                .await
                .unwrap()
                .unwrap()
                .deactivated
        );

        // A person still gets the UIA challenge.
        let person = user_id!("@carol:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(person.clone(), 0))
            .await
            .unwrap();
        let response = post_account_deactivate(
            State(state.clone()),
            Requester::for_user(person.clone()),
            PermissiveJson(json!({})),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !state
                .store
                .get_user(&person)
                .await
                .unwrap()
                .unwrap()
                .deactivated
        );
    }

    #[tokio::test]
    async fn deactivate_with_erase_erases_the_account() {
        let (state, requester) = state_with_user("oldpassword1").await;
        state
            .store
            .set_profile_display_name(&requester.user_id, Some("Alice".into()))
            .await
            .unwrap();
        let body = json!({"erase": true, "auth": {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "oldpassword1"}});
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
        assert!(user.deactivated && user.erased);
        assert!(user.erased_at_ms.is_some());
        assert_eq!(user.display_name, None);
        assert!(
            state
                .store
                .list_devices(&requester.user_id)
                .await
                .unwrap()
                .is_empty()
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
        let (state, requester) = state_with_user("hunter2345").await;
        let Json(body) = get_account_3pid(State(state), requester).await.unwrap();
        assert_eq!(
            body["threepids"],
            json!([]),
            "threepids must be present and an empty array, not absent: {body}"
        );
    }

    /// A 3PID an administrator bound is listed with both timestamps the spec requires.
    #[tokio::test]
    async fn account_3pid_lists_what_an_administrator_bound() {
        let (state, requester) = state_with_user("hunter2345").await;
        state
            .store
            .add_threepid(crate::store::ThreepidRecord {
                user_id: requester.user_id.clone(),
                medium: "email".to_owned(),
                address: "alice@example.org".to_owned(),
                added_at_ms: 1_000,
                validated_at_ms: 1_000,
            })
            .await
            .unwrap();
        let Json(body) = get_account_3pid(State(state), requester).await.unwrap();
        assert_eq!(
            body["threepids"],
            json!([{"medium": "email", "address": "alice@example.org", "added_at": 1000, "validated_at": 1000}])
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
