//! `POST /logout` and `POST /logout/all`.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::error::MatrixError;
use crate::middleware::AllowGuest;
use crate::state::AuthState;

/// `POST /logout`: invalidates the access token used for this request, and its paired refresh
/// token if it had one. Guests may log out (matching Synapse's `allow_guest=True` here), so this
/// takes [`AllowGuest`] rather than [`crate::requester::Requester`].
pub async fn post_logout(
    State(state): State<AuthState>,
    AllowGuest(requester): AllowGuest,
) -> Result<Json<Value>, MatrixError> {
    if let Some(hash) = requester.access_token_id {
        if let Some(record) = state.store.get_access_token(&hash).await?
            && let Some(refresh_hash) = record.refresh_token_hash
        {
            state.store.delete_refresh_token(&refresh_hash).await?;
        }
        state.store.delete_access_token(&hash).await?;
    }
    Ok(Json(json!({})))
}

/// `POST /logout/all`: invalidates every access and refresh token for the requesting user, across
/// every device.
pub async fn post_logout_all(
    State(state): State<AuthState>,
    AllowGuest(requester): AllowGuest,
) -> Result<Json<Value>, MatrixError> {
    state
        .store
        .delete_all_access_tokens_for_user(&requester.user_id)
        .await?;
    state
        .store
        .delete_all_refresh_tokens_for_user(&requester.user_id)
        .await?;
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AccessTokenRecord, RefreshTokenRecord, UserRecord};
    use crate::token::TokenHash;
    use ruma::{device_id, user_id};

    #[tokio::test]
    async fn logout_deletes_the_token_used_and_its_refresh_pair() {
        let state = AuthState::in_memory();
        let uid = user_id!("@alice:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();

        let access_hash = TokenHash::of("syt_a");
        let refresh_hash = TokenHash::of("syr_a");
        state
            .store
            .put_access_token(AccessTokenRecord {
                hash: access_hash,
                user_id: uid.clone(),
                device_id: Some(device_id!("D1").to_owned()),
                expires_at_ms: None,
                refresh_token_hash: Some(refresh_hash),
                last_used_ms: None,
            })
            .await
            .unwrap();
        state
            .store
            .put_refresh_token(RefreshTokenRecord {
                hash: refresh_hash,
                user_id: uid.clone(),
                device_id: device_id!("D1").to_owned(),
                access_token_hash: access_hash,
                used: false,
                replaced_by: None,
                expires_at_ms: None,
                ultimate_session_expiry_ms: None,
            })
            .await
            .unwrap();

        let requester = crate::requester::Requester {
            access_token_id: Some(access_hash),
            ..crate::requester::Requester::for_user(uid)
        };
        let _ = post_logout(State(state.clone()), AllowGuest(requester))
            .await
            .unwrap();

        assert!(
            state
                .store
                .get_access_token(&access_hash)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .store
                .get_refresh_token(&refresh_hash)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn logout_all_clears_every_token_for_the_user() {
        let state = AuthState::in_memory();
        let uid = user_id!("@bob:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        for i in 0..3 {
            let token = format!("syt_{i}");
            state
                .store
                .put_access_token(AccessTokenRecord {
                    hash: TokenHash::of(&token),
                    user_id: uid.clone(),
                    device_id: None,
                    expires_at_ms: None,
                    refresh_token_hash: None,
                    last_used_ms: None,
                })
                .await
                .unwrap();
        }
        let requester = crate::requester::Requester::for_user(uid.clone());
        let _ = post_logout_all(State(state.clone()), AllowGuest(requester))
            .await
            .unwrap();
        assert_eq!(
            state
                .store
                .delete_all_access_tokens_for_user(&uid)
                .await
                .unwrap(),
            0
        );
    }
}
