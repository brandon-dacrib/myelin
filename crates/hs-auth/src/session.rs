//! Minting a fresh access/refresh token pair and device for a just-authenticated user. Shared by
//! `POST /login` (every `m.login.*` type) and `POST /register` (on successful UIA completion,
//! unless `inhibit_login` was set), so the two endpoints cannot drift on token shape, expiry or
//! device bookkeeping.

use ruma::{OwnedDeviceId, UserId};

use crate::error::MatrixError;
use crate::state::AuthState;
use crate::store::{AccessTokenRecord, DeviceRecord, RefreshTokenRecord};
use crate::token::{self, TokenHash};

/// The tokens and device id to hand back to the client.
pub struct NewSession {
    /// The fresh access token.
    pub access_token: String,
    /// The fresh refresh token, if one was requested.
    pub refresh_token: Option<String>,
    /// The device this session is bound to (freshly generated if the client did not name one).
    pub device_id: OwnedDeviceId,
    /// The access token's lifetime, if it has one.
    pub expires_in_ms: Option<u64>,
}

/// Creates a device (or refreshes the seen-time of an existing one with this id) and mints a
/// fresh access token, and a refresh token if `refresh` is set.
///
/// # Errors
/// Propagates storage errors as [`MatrixError::internal`] (logged).
pub async fn create_session(
    state: &AuthState,
    user_id: &UserId,
    device_id: Option<OwnedDeviceId>,
    initial_device_display_name: Option<String>,
    refresh: bool,
) -> Result<NewSession, MatrixError> {
    let now = state.now_ms();
    let device_id = device_id.unwrap_or_else(ruma::DeviceId::new);

    match state.store.get_device(user_id, &device_id).await? {
        Some(_) => {
            state
                .store
                .record_seen(user_id, &device_id, now, None)
                .await?;
        }
        None => {
            state
                .store
                .upsert_device(DeviceRecord {
                    user_id: user_id.to_owned(),
                    device_id: device_id.clone(),
                    display_name: initial_device_display_name,
                    last_seen_ms: Some(now),
                    last_seen_ip: None,
                })
                .await?;
        }
    }

    let localpart = user_id.localpart();
    let access_token = token::generate_access_token(localpart);
    let access_hash = TokenHash::of(&access_token);

    let (refresh_token, refresh_hash, expires_in_ms) = if refresh {
        let rt = token::generate_refresh_token(localpart);
        let rt_hash = TokenHash::of(&rt);
        (
            Some(rt),
            Some(rt_hash),
            Some(state.config.refreshable_access_token_ttl_ms),
        )
    } else {
        (None, None, state.config.nonrefreshable_access_token_ttl_ms)
    };

    state
        .store
        .put_access_token(AccessTokenRecord {
            hash: access_hash,
            user_id: user_id.to_owned(),
            device_id: Some(device_id.clone()),
            expires_at_ms: expires_in_ms.map(|ms| now + ms),
            refresh_token_hash: refresh_hash,
            last_used_ms: Some(now),
        })
        .await?;

    if let Some(rt_hash) = refresh_hash {
        state
            .store
            .put_refresh_token(RefreshTokenRecord {
                hash: rt_hash,
                user_id: user_id.to_owned(),
                device_id: device_id.clone(),
                access_token_hash: access_hash,
                used: false,
                replaced_by: None,
                expires_at_ms: state.config.refresh_token_ttl_ms.map(|ms| now + ms),
                ultimate_session_expiry_ms: state.config.session_lifetime_ms.map(|ms| now + ms),
            })
            .await?;
    }

    Ok(NewSession {
        access_token,
        refresh_token,
        device_id,
        expires_in_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::UserRecord;
    use ruma::user_id;

    #[tokio::test]
    async fn creates_a_new_device_when_none_named() {
        let state = AuthState::in_memory();
        let uid = user_id!("@alice:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let session = create_session(&state, &uid, None, Some("phone".into()), false)
            .await
            .unwrap();
        let device = state
            .store
            .get_device(&uid, &session.device_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(device.display_name.as_deref(), Some("phone"));
        assert!(session.refresh_token.is_none());
        assert!(session.expires_in_ms.is_none());
    }

    #[tokio::test]
    async fn refresh_true_mints_a_refresh_token_and_sets_expiry() {
        let state = AuthState::in_memory();
        let uid = user_id!("@bob:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let session = create_session(&state, &uid, None, None, true)
            .await
            .unwrap();
        assert!(session.refresh_token.is_some());
        assert!(session.expires_in_ms.is_some());

        let access_hash = TokenHash::of(&session.access_token);
        let record = state
            .store
            .get_access_token(&access_hash)
            .await
            .unwrap()
            .unwrap();
        assert!(record.expires_at_ms.is_some());
        assert!(record.refresh_token_hash.is_some());
    }

    #[tokio::test]
    async fn reusing_a_device_id_does_not_clear_its_display_name() {
        let state = AuthState::in_memory();
        let uid = user_id!("@carol:example.org").to_owned();
        state
            .store
            .create_user(UserRecord::new(uid.clone(), 0))
            .await
            .unwrap();
        let first = create_session(&state, &uid, None, Some("first name".into()), false)
            .await
            .unwrap();
        // Log in again with the same device id, no display name supplied this time.
        let second = create_session(&state, &uid, Some(first.device_id.clone()), None, false)
            .await
            .unwrap();
        assert_eq!(first.device_id, second.device_id);
        let device = state
            .store
            .get_device(&uid, &second.device_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(device.display_name.as_deref(), Some("first name"));
    }
}
