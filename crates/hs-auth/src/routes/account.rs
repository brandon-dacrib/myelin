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

use axum::http::StatusCode;
use ruma::api::client::uiaa::{AuthData, AuthFlow, AuthType};

use crate::error::MatrixError;
use crate::middleware::MaybeRequester;
use crate::password;
use crate::reauth;
use crate::requester::Requester;
use crate::state::AuthState;
use crate::uia;
use hs_http::body::PermissiveJson;

/// `POST /account/password`: with an access token, the caller changes their own password after
/// re-authenticating ([`reauth`]); without one, somebody resets the password of the account
/// whose email address they prove they control (`m.login.email.identity`, see
/// [`reset_password_by_email`]) -- the two cases of Synapse's `PasswordRestServlet`.
pub async fn post_account_password(
    State(state): State<AuthState>,
    MaybeRequester(requester): MaybeRequester,
    PermissiveJson(body): PermissiveJson<Value>,
) -> Result<Response, MatrixError> {
    match requester {
        Some(requester) => change_password(state, requester, body).await,
        None => reset_password_by_email(state, body).await,
    }
}

/// Where a password reset's UIA session remembers the email address its
/// `m.login.email.identity` stage proved.
const RESET_ADDRESS_KEY: &str = "password_reset_address";

/// Where a password reset's UIA session remembers the new password's hash, for a later round
/// that does not send the password again (as Synapse's `PASSWORD_HASH` session data).
const RESET_PASSWORD_HASH_KEY: &str = "password_reset_hash";

/// The signed-out half of `POST /account/password`: one user-interactive auth flow,
/// `[m.login.email.identity]`, whose `threepid_creds` name a validated password-reset session
/// (`POST /account/password/email/requestToken`, its emailed link followed and confirmed:
/// [`crate::threepid`]). Once it completes, the account that address belongs to gets the new
/// password and, unless `logout_devices` is `false`, loses every session. A server that cannot
/// send email has no way to reset a password, so it answers as before, `401
/// M_MISSING_TOKEN`.
async fn reset_password_by_email(state: AuthState, body: Value) -> Result<Response, MatrixError> {
    if !crate::threepid::email_available(&state) {
        return Err(MatrixError::missing_token());
    }
    let new_password = body.get("new_password").and_then(Value::as_str);
    if let Some(new_password) = new_password {
        state.config.get().password_policy.validate(new_password)?;
    }
    let flows = vec![AuthFlow::new(vec![AuthType::EmailIdentity])];
    let auth: Option<AuthData> = match body.get("auth") {
        Some(v) if !v.is_null() => Some(
            serde_json::from_value(v.clone())
                .map_err(|_| MatrixError::invalid_param("invalid auth data"))?,
        ),
        _ => None,
    };
    let store = state.store.as_ref();
    let timeout = state.config.get().uia_session_timeout_ms;
    let session_id = uia::session_id_for(
        store,
        auth.as_ref().and_then(AuthData::session),
        state.now_ms(),
        timeout,
    )
    .await?;
    uia::bind_operation(store, &session_id, "POST /account/password").await?;
    let submitted_type = auth.as_ref().and_then(AuthData::auth_type);
    let stage_ok = match &auth {
        Some(AuthData::EmailIdentity(e)) => {
            let creds = &e.thirdparty_id_creds;
            match crate::threepid::password_reset_address(
                &state,
                creds.sid.as_str(),
                creds.client_secret.as_str(),
            )
            .await?
            {
                Some(address) => {
                    store
                        .set_session_data(&session_id, RESET_ADDRESS_KEY, json!(address))
                        .await?;
                    true
                }
                None => false,
            }
        }
        _ => submitted_type.is_none(),
    };
    let outcome = uia::advance(
        store,
        &flows,
        Some(&session_id),
        submitted_type,
        stage_ok,
        state.now_ms(),
        timeout,
    )
    .await?;
    let remembered_hash = store
        .get_session_data(&session_id, RESET_PASSWORD_HASH_KEY)
        .await?
        .and_then(|v| v.as_str().map(str::to_owned));

    if !outcome.complete {
        if let Some(new_password) = new_password
            && remembered_hash.is_none()
        {
            let hash =
                password::hash_password(new_password).map_err(|_| MatrixError::internal())?;
            store
                .set_session_data(&session_id, RESET_PASSWORD_HASH_KEY, json!(hash))
                .await?;
        }
        let body = uia::incomplete_body(flows, outcome.completed, outcome.session_id);
        return Ok((StatusCode::UNAUTHORIZED, Json(body)).into_response());
    }

    let address = store
        .get_session_data(&session_id, RESET_ADDRESS_KEY)
        .await?
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(MatrixError::internal)?;
    let user_id = crate::threepid::password_reset_owner(&state, &address).await?;
    let user = state
        .store
        .get_user(&user_id)
        .await?
        .ok_or_else(MatrixError::internal)?;
    if user.deactivated {
        return Err(MatrixError::forbidden("This account has been deactivated"));
    }
    let hash = match (new_password, remembered_hash) {
        (Some(new_password), _) => {
            password::hash_password(new_password).map_err(|_| MatrixError::internal())?
        }
        (None, Some(hash)) => hash,
        (None, None) => return Err(MatrixError::missing_param("Missing params: password")),
    };
    state.store.set_password_hash(&user_id, Some(hash)).await?;
    let logout_devices = body
        .get("logout_devices")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if logout_devices {
        state
            .store
            .delete_all_access_tokens_for_user(&user_id)
            .await?;
        state
            .store
            .delete_all_refresh_tokens_for_user(&user_id)
            .await?;
        state.notify_other_sessions_revoked(&user_id, None).await;
    }
    crate::threepid::count_password_reset("reset");
    tracing::info!(user = %user_id, logout_devices, "a password was reset by email");
    Ok(Json(json!({})).into_response())
}

/// The signed-in half of `POST /account/password`.
async fn change_password(
    state: AuthState,
    requester: Requester,
    body: Value,
) -> Result<Response, MatrixError> {
    requester.require_not_suspended()?;

    let new_password = body
        .get("new_password")
        .and_then(Value::as_str)
        .ok_or_else(|| MatrixError::missing_param("Missing new_password"))?;
    state.config.get().password_policy.validate(new_password)?;

    if let Some(response) =
        reauth::run_for_operation(&state, &requester, &body, "POST /account/password").await?
    {
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

/// Leaves every room `user_id` is in through the installed [`crate::state::RoomDeparture`],
/// logging what happened; nothing without one (a test of this crate alone, or a process without
/// a room layer).
async fn leave_rooms(state: &AuthState, user_id: &ruma::UserId) {
    let Some(departure) = state.room_departure() else {
        tracing::debug!(user = %user_id, "no room layer is installed; a deactivated account's rooms keep it as a member");
        return;
    };
    match departure.leave_all_rooms(user_id).await {
        Ok(report) => {
            for (room_id, reason) in &report.rooms_failed {
                tracing::warn!(user = %user_id, room = %room_id, %reason, "a deactivated account could not leave a room");
            }
            tracing::info!(
                user = %user_id,
                rooms_left = report.rooms_left.len(),
                rooms_failed = report.rooms_failed.len(),
                "a deactivated account left its rooms"
            );
        }
        Err(error) => {
            tracing::warn!(user = %user_id, %error, "a deactivated account's rooms could not be left; it stays in them");
        }
    }
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
    } else if let Some(response) =
        reauth::run_for_operation(&state, &requester, &body, "POST /account/deactivate").await?
    {
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

    // The account leaves every room it is in, as Synapse's deactivation parts it from them
    // (`DeactivateAccountHandler._part_user`), erased or not: through the room layer's hook when
    // `hs serve` installed one (`crate::state::RoomDeparture`), before any erasure so that the
    // leaves are authored by an account that still has its profile. A room that cannot be left
    // does not fail the deactivation: it is logged, and the account is deactivated either way.
    leave_rooms(&state, &requester.user_id).await;

    // The spec's `erase` (MSC2438): the same erasure an administrator's `users.deactivate`
    // with `erase: true` performs (see `crate::erasure`); the devices' keys go with the
    // devices, through the device-list hook.
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

    // The account's third-party identifiers, as Synapse's deactivation handles them: each one
    // this server bound at an identity server is unbound there, and every one bound to the
    // account here is removed (`crate::threepid::on_deactivation`). `success` when every
    // unbind succeeded -- and so when there was nothing to unbind; `no-support` when an
    // identity server does not support unbinding or none could be asked. The erasure above
    // already removed the local ones.
    let unbound = crate::threepid::on_deactivation(&state, &requester.user_id).await?;
    Ok(Json(json!({
        "id_server_unbind_result": if unbound { "success" } else { "no-support" }
    }))
    .into_response())
}

/// `GET /account/3pid`: the third-party identifiers (email addresses, phone numbers) this
/// homeserver has associated with the caller's account.
///
/// An administrator binds them (`users.threepids.add` in the admin API,
/// [`crate::store::IdentityStore::add_threepid`]), and since 2026-10-04 so can the user, once
/// this server has validated the address (`POST /account/3pid/add`, [`crate::threepid`]). Each
/// is listed with the `added_at`/`validated_at` timestamps the spec makes required.
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
    use ruma::user_id;
    use std::sync::Arc;

    /// The signed-in call, as every test below but the email reset makes it.
    async fn post_account_password(
        state: State<AuthState>,
        requester: Requester,
        body: PermissiveJson<Value>,
    ) -> Result<Response, MatrixError> {
        super::post_account_password(state, MaybeRequester(Some(requester)), body).await
    }

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

    /// A room layer that records whom it was asked to part, and what the profile was then.
    struct RecordingDeparture {
        asked: std::sync::Mutex<Vec<(String, Option<String>)>>,
        store: Arc<dyn crate::store::AuthStore>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl crate::state::RoomDeparture for RecordingDeparture {
        async fn leave_all_rooms(
            &self,
            user_id: &ruma::UserId,
        ) -> Result<crate::state::RoomDepartureReport, String> {
            let name = self
                .store
                .get_user(user_id)
                .await
                .unwrap()
                .and_then(|u| u.display_name);
            self.asked.lock().unwrap().push((user_id.to_string(), name));
            if self.fail {
                return Err("the room store is away".to_owned());
            }
            Ok(crate::state::RoomDepartureReport {
                rooms_left: vec!["!lobby:example.org".to_owned()],
                rooms_failed: vec![("!stuck:example.org".to_owned(), "no server".to_owned())],
            })
        }
    }

    #[tokio::test]
    async fn deactivation_leaves_the_rooms_before_erasing_and_survives_a_room_layer_failure() {
        for fail in [false, true] {
            let (state, requester) = state_with_user("oldpassword1").await;
            state
                .store
                .set_profile_display_name(&requester.user_id, Some("Alice".into()))
                .await
                .unwrap();
            let departure = Arc::new(RecordingDeparture {
                asked: std::sync::Mutex::new(Vec::new()),
                store: state.store.clone(),
                fail,
            });
            state.install_room_departure(departure.clone());
            let body = json!({"erase": true, "auth": {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "alice"}, "password": "oldpassword1"}});
            let response = post_account_deactivate(
                State(state.clone()),
                requester.clone(),
                PermissiveJson(body),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "fail={fail}");
            // Asked once, for this account, while its profile was still there (the erasure
            // comes after, so a leave event is stamped with the name).
            assert_eq!(
                *departure.asked.lock().unwrap(),
                vec![(requester.user_id.to_string(), Some("Alice".to_owned()))],
                "fail={fail}"
            );
            let user = state
                .store
                .get_user(&requester.user_id)
                .await
                .unwrap()
                .unwrap();
            assert!(user.deactivated && user.erased, "fail={fail}");
            assert_eq!(user.display_name, None, "fail={fail}");
        }
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
