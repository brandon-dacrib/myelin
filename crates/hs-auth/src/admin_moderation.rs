//! [`AuthStoreUserModeration`]: `hs_admin::user_moderation::UserModerationSource` over this
//! crate's own [`crate::store::AuthStore`] -- the account side of the admin API's user
//! moderation operations: the suspended and shadow-banned flags, the per-user rate-limit
//! override, the user's sessions, and minting a support session (`users.login_as`).
//!
//! The flags are only recorded here. Suspension is enforced by every write route calling
//! [`crate::requester::Requester::require_not_suspended`] (the room routes, the profile
//! routes, media uploads); a shadow-ban by the room routes, which read
//! [`crate::requester::Requester::shadow_banned`]; and the override by the room routes' message
//! limiter, which reads [`crate::store::UserRecord::rate_limit_override`].

use hs_admin::sources::SourceError;
use hs_admin::user_moderation::{
    AdminSession, RateLimitOverride, SupportToken, UserModerationSource,
};

use crate::admin_verifier::format_rfc3339_ms;
use crate::state::AuthState;
use crate::store::{
    AccessTokenRecord, DeviceRecord, RateLimitOverrideRecord, StoreError, UserRecord,
};
use crate::token::{self, TokenHash};

/// Every support session's device id starts with this, which is how
/// [`AuthStoreUserModeration::sessions`] tells one apart from the user's own devices. A client
/// cannot rename a device id, so the mark stays with the session for as long as it exists.
pub const SUPPORT_DEVICE_PREFIX: &str = "ADMINSUPPORT";

/// The account side of user moderation, over the same store (and clock, and device-list
/// announcements) as the client-server routes.
pub struct AuthStoreUserModeration {
    state: AuthState,
}

impl AuthStoreUserModeration {
    /// Builds the source over `state`'s store.
    #[must_use]
    pub fn from_auth_state(state: &AuthState) -> Self {
        Self {
            state: state.clone(),
        }
    }

    async fn user(&self, user_id: &str) -> Result<UserRecord, SourceError> {
        let uid = parse_user_id(user_id)?;
        self.state
            .store
            .get_user(&uid)
            .await
            .map_err(unavailable)?
            .ok_or(SourceError::NotFound)
    }
}

fn parse_user_id(user_id: &str) -> Result<ruma::OwnedUserId, SourceError> {
    ruma::UserId::parse(user_id).map_err(|e| SourceError::Invalid(e.to_string()))
}

fn unavailable(err: StoreError) -> SourceError {
    SourceError::Unavailable(err.to_string())
}

fn map_set_error(err: StoreError) -> SourceError {
    match err {
        StoreError::NotFound(_) => SourceError::NotFound,
        other => unavailable(other),
    }
}

/// The admin API's shape of a stored override.
fn to_wire(record: RateLimitOverrideRecord) -> RateLimitOverride {
    RateLimitOverride {
        messages_per_second: Some(record.per_second),
        burst_count: Some(record.burst_count),
    }
}

#[async_trait::async_trait]
impl UserModerationSource for AuthStoreUserModeration {
    async fn set_suspended(&self, user_id: &str, suspended: bool) -> Result<(), SourceError> {
        let uid = parse_user_id(user_id)?;
        self.state
            .store
            .set_suspended(&uid, suspended)
            .await
            .map_err(map_set_error)
    }

    async fn set_shadow_banned(
        &self,
        user_id: &str,
        shadow_banned: bool,
    ) -> Result<(), SourceError> {
        let uid = parse_user_id(user_id)?;
        self.state
            .store
            .set_shadow_banned(&uid, shadow_banned)
            .await
            .map_err(map_set_error)
    }

    async fn rate_limit(&self, user_id: &str) -> Result<Option<RateLimitOverride>, SourceError> {
        Ok(self.user(user_id).await?.rate_limit_override.map(to_wire))
    }

    async fn set_rate_limit(
        &self,
        user_id: &str,
        rate_limit: Option<RateLimitOverride>,
    ) -> Result<(), SourceError> {
        let uid = parse_user_id(user_id)?;
        let record = match rate_limit {
            None => None,
            Some(wanted) => Some(RateLimitOverrideRecord {
                per_second: wanted.messages_per_second.ok_or_else(|| {
                    SourceError::InvalidField {
                        pointer: "/messages_per_second",
                        detail: "messages_per_second is required".to_owned(),
                    }
                })?,
                burst_count: wanted
                    .burst_count
                    .unwrap_or(hs_admin::user_moderation::DEFAULT_OVERRIDE_BURST),
            }),
        };
        self.state
            .store
            .set_rate_limit_override(&uid, record)
            .await
            .map_err(map_set_error)
    }

    async fn sessions(&self, user_id: &str) -> Result<Vec<AdminSession>, SourceError> {
        let record = self.user(user_id).await?;
        let mut devices = self
            .state
            .store
            .list_devices(&record.user_id)
            .await
            .map_err(unavailable)?;
        devices.sort_by_key(|d| std::cmp::Reverse(d.last_seen_ms));
        Ok(devices
            .into_iter()
            .map(|d| AdminSession {
                support_session: d.device_id.as_str().starts_with(SUPPORT_DEVICE_PREFIX),
                device_id: d.device_id.to_string(),
                display_name: d.display_name,
                ip: d.last_seen_ip,
                // Not recorded by this server: no request records its client's user agent.
                user_agent: None,
                created_at: None,
                last_seen_at: d.last_seen_ms.map(format_rfc3339_ms),
            })
            .collect())
    }

    /// A new device named for the administrator who asked, and an access token for it that
    /// expires after `valid_for_ms` and cannot be refreshed. Everyone who shares a room with the
    /// user is told their device list changed, as for any new sign-in.
    async fn mint_support_token(
        &self,
        user_id: &str,
        valid_for_ms: u64,
        minted_by: &str,
    ) -> Result<SupportToken, SourceError> {
        let record = self.user(user_id).await?;
        if record.deactivated {
            return Err(SourceError::Conflict(format!(
                "{user_id} is deactivated; nobody can act as it"
            )));
        }
        let uid = record.user_id;
        let now = self.state.now_ms();
        let device_id: ruma::OwnedDeviceId =
            format!("{SUPPORT_DEVICE_PREFIX}{}", ruma::DeviceId::new()).into();
        self.state
            .store
            .upsert_device(DeviceRecord {
                user_id: uid.clone(),
                device_id: device_id.clone(),
                display_name: Some(format!("Support session for {minted_by}")),
                last_seen_ms: Some(now),
                last_seen_ip: None,
            })
            .await
            .map_err(unavailable)?;
        let access_token = token::generate_access_token(uid.localpart());
        let expires_at_ms = now.saturating_add(valid_for_ms);
        self.state
            .store
            .put_access_token(AccessTokenRecord {
                hash: TokenHash::of(&access_token),
                user_id: uid.clone(),
                device_id: Some(device_id.clone()),
                expires_at_ms: Some(expires_at_ms),
                refresh_token_hash: None,
                last_used_ms: Some(now),
            })
            .await
            .map_err(unavailable)?;
        self.state.notify_device_list_changed(&uid).await;
        Ok(SupportToken {
            user_id: uid.to_string(),
            access_token,
            device_id: device_id.to_string(),
            expires_at: format_rfc3339_ms(expires_at_ms),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    async fn source() -> (AuthState, AuthStoreUserModeration) {
        let state = AuthState::in_memory();
        state
            .store
            .create_user(UserRecord::new(
                user_id!("@alice:example.org").to_owned(),
                0,
            ))
            .await
            .unwrap();
        let source = AuthStoreUserModeration::from_auth_state(&state);
        (state, source)
    }

    #[tokio::test]
    async fn flags_and_override_reach_the_account_record() {
        let (state, source) = source().await;
        source
            .set_suspended("@alice:example.org", true)
            .await
            .unwrap();
        source
            .set_shadow_banned("@alice:example.org", true)
            .await
            .unwrap();
        source
            .set_rate_limit(
                "@alice:example.org",
                Some(RateLimitOverride {
                    messages_per_second: Some(0.0),
                    burst_count: None,
                }),
            )
            .await
            .unwrap();
        let record = state
            .store
            .get_user(user_id!("@alice:example.org"))
            .await
            .unwrap()
            .unwrap();
        assert!(record.suspended);
        assert!(record.shadow_banned);
        assert_eq!(
            record.rate_limit_override,
            Some(RateLimitOverrideRecord {
                per_second: 0.0,
                burst_count: hs_admin::user_moderation::DEFAULT_OVERRIDE_BURST,
            })
        );
        assert_eq!(
            source
                .rate_limit("@alice:example.org")
                .await
                .unwrap()
                .unwrap()
                .messages_per_second,
            Some(0.0)
        );
        source
            .set_rate_limit("@alice:example.org", None)
            .await
            .unwrap();
        assert_eq!(source.rate_limit("@alice:example.org").await.unwrap(), None);
        assert!(matches!(
            source.set_suspended("@ghost:example.org", true).await,
            Err(SourceError::NotFound)
        ));
        assert!(matches!(
            source.sessions("@ghost:example.org").await,
            Err(SourceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_support_token_authenticates_as_the_user_until_it_expires() {
        let (state, source) = source().await;
        let token = source
            .mint_support_token("@alice:example.org", 60_000, "@ops:example.org")
            .await
            .unwrap();
        assert!(token.device_id.starts_with(SUPPORT_DEVICE_PREFIX));
        let record = state
            .store
            .get_access_token(&TokenHash::of(&token.access_token))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.user_id, user_id!("@alice:example.org"));
        assert_eq!(
            record.expires_at_ms,
            Some(record.last_used_ms.unwrap() + 60_000)
        );
        assert!(record.refresh_token_hash.is_none());
        let sessions = source.sessions("@alice:example.org").await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].support_session);
        assert_eq!(
            sessions[0].display_name.as_deref(),
            Some("Support session for @ops:example.org")
        );

        state
            .store
            .set_deactivated(user_id!("@alice:example.org"), true)
            .await
            .unwrap();
        assert!(matches!(
            source
                .mint_support_token("@alice:example.org", 60_000, "@ops:example.org")
                .await,
            Err(SourceError::Conflict(_))
        ));
    }
}
