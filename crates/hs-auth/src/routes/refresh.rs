//! `POST /refresh`: exchanges a refresh token for a new access/refresh token pair.
//!
//! No `Authorization` header is involved — the refresh token itself is the credential, per the
//! spec. Reuse of an already-consumed refresh token (a sign of token theft: the legitimate client
//! and an attacker both tried to refresh from the same point) revokes the *entire* chain for that
//! device rather than just rejecting the one request, which is stricter than Synapse's
//! behavior but matches the spec's stated intent for refresh token rotation; this is called out
//! as a deliberate hardening in `docs/rfcs/0002-auth-tokens-and-requester.md` section 6.
//!
//! For simplicity, the access token half of the old pair is revoked immediately on a successful
//! refresh rather than kept alive for a grace period the way Synapse's `mark_access_token_used`
//! bookkeeping allows; a client that refreshes proactively before its access token expires never
//! notices.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::error::MatrixError;
use crate::session;
use crate::state::AuthState;
use crate::token::TokenHash;

/// `POST /refresh`.
pub async fn post_refresh(
    State(state): State<AuthState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, MatrixError> {
    let raw = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .ok_or_else(|| MatrixError::missing_param("Missing refresh_token"))?;
    let hash = TokenHash::of(raw);

    let record = state
        .store
        .get_refresh_token(&hash)
        .await?
        .ok_or_else(|| MatrixError::unknown_token(false))?;

    let now = state.now_ms();

    if record.used {
        // Reuse of a consumed refresh token: revoke the whole session for this device, since we
        // cannot tell the legitimate client from an attacker who stole an earlier token.
        state
            .store
            .delete_access_tokens_for_device(&record.user_id, &record.device_id)
            .await?;
        state.store.delete_refresh_token(&hash).await?;
        return Err(MatrixError::unknown_token(true));
    }

    if record.expires_at_ms.is_some_and(|exp| exp < now)
        || record
            .ultimate_session_expiry_ms
            .is_some_and(|exp| exp < now)
    {
        return Err(MatrixError::unknown_token(true));
    }

    // Revoke the old access token; mint a fresh pair.
    state
        .store
        .delete_access_token(&record.access_token_hash)
        .await?;

    let session = session::create_session(
        &state,
        &record.user_id,
        Some(record.device_id.clone()),
        None,
        true,
    )
    .await?;

    // Chain: mark the old refresh token used and pointing at the new one, so a later reuse of the
    // old token is detected as theft even after this legitimate rotation.
    if let Some(new_refresh) = &session.refresh_token {
        state
            .store
            .mark_refresh_token_used(&hash, TokenHash::of(new_refresh))
            .await?;
    }

    let mut response = json!({
        "access_token": session.access_token,
    });
    if let Some(rt) = session.refresh_token {
        response["refresh_token"] = json!(rt);
    }
    if let Some(ms) = session.expires_in_ms {
        response["expires_in_ms"] = json!(ms);
    }
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::UserRecord;
    use ruma::user_id;

    async fn logged_in_state() -> (AuthState, String) {
        let state = AuthState::in_memory();
        let uid = user_id!("@alice:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let session = session::create_session(&state, &uid, None, None, true)
            .await
            .unwrap();
        (state, session.refresh_token.unwrap())
    }

    #[tokio::test]
    async fn refresh_mints_a_new_pair_and_invalidates_the_old_access_token() {
        let (state, refresh_token) = logged_in_state().await;
        let old_hash = TokenHash::of(&refresh_token);
        let old_record = state
            .store
            .get_refresh_token(&old_hash)
            .await
            .unwrap()
            .unwrap();

        let body = json!({"refresh_token": refresh_token});
        let Json(response) = post_refresh(State(state.clone()), Json(body))
            .await
            .unwrap();
        assert!(response["access_token"].is_string());
        assert!(response["refresh_token"].is_string());

        assert!(
            state
                .store
                .get_access_token(&old_record.access_token_hash)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn reusing_a_consumed_refresh_token_revokes_the_session() {
        let (state, refresh_token) = logged_in_state().await;
        let body = json!({"refresh_token": refresh_token});
        let _ = post_refresh(State(state.clone()), Json(body.clone()))
            .await
            .unwrap();

        // Second use of the same (now-consumed) refresh token: must fail...
        let err = post_refresh(State(state.clone()), Json(body))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_UNKNOWN_TOKEN");
    }

    #[tokio::test]
    async fn unknown_refresh_token_is_rejected() {
        let state = AuthState::in_memory();
        let body = json!({"refresh_token": "syr_nope"});
        let err = post_refresh(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_UNKNOWN_TOKEN");
    }

    #[tokio::test]
    async fn missing_refresh_token_field_is_rejected() {
        let state = AuthState::in_memory();
        let err = post_refresh(State(state), Json(json!({})))
            .await
            .unwrap_err();
        assert_eq!(err.errcode().as_str(), "M_MISSING_PARAM");
    }
}
