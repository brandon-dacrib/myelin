//! The reference client for the HTTP-callback protocol (`crate::callback`): given a base URL, it
//! implements [`ModuleHooks`] by calling out over HTTP, falling back to each hook's permissive
//! default (matching [`crate::noop::NoopHooks`]) if the module doesn't implement that hook
//! (`404`) or is unreachable. A module outage degrades to "no opinion", not a server outage.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::callback::{CallbackRequest, CallbackResponse, UnsupportedVersion, hook_names};
use crate::hooks::{
    AuthResult, BackgroundUpdateGuidance, CheckResult, EventForCheck, MediaDescriptor, ModuleHooks,
    PresenceInterest, RatelimitOverride, RuleResult, UserProfile,
};

#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    #[error("transport error calling module: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("the module speaks an unsupported protocol version; it supports {0:?}")]
    UnsupportedVersion(Vec<String>),
    #[error("unexpected HTTP status from module: {0}")]
    UnexpectedStatus(u16),
}

/// Calls an HTTP-callback module. One client per configured module (a server with several
/// modules holds several clients, composed with [`crate::noop::ModuleChain`]).
pub struct HttpCallbackClient {
    http: reqwest::Client,
    base_url: String,
}

impl HttpCallbackClient {
    /// `base_url` with no trailing slash, e.g. `http://spam-checker.internal:8080`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_timeout(base_url, Duration::from_secs(2))
    }

    pub fn with_timeout(base_url: impl Into<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Calls one hook. `Ok(None)` means the module does not implement this hook (`404`), which
    /// callers treat as "no opinion", not an error.
    pub async fn call<Req, Resp>(
        &self,
        hook: &str,
        payload: Req,
    ) -> Result<Option<Resp>, CallbackError>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let url = format!("{}/{hook}", self.base_url);
        let body = CallbackRequest::new(payload);
        let response = self.http.post(&url).json(&body).send().await?;
        match response.status() {
            reqwest::StatusCode::OK => {
                let envelope: CallbackResponse<Resp> = response.json().await?;
                Ok(Some(envelope.result))
            }
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            reqwest::StatusCode::CONFLICT => {
                let unsupported = response
                    .json::<UnsupportedVersion>()
                    .await
                    .unwrap_or_else(|_| UnsupportedVersion::current());
                Err(CallbackError::UnsupportedVersion(unsupported.supported))
            }
            other => Err(CallbackError::UnexpectedStatus(other.as_u16())),
        }
    }

    /// Like [`Self::call`], but logs and falls back to `default` on any error instead of
    /// propagating it: the shape every [`ModuleHooks`] method below uses, so one unreachable
    /// module cannot take down the server.
    async fn call_or_default<Req, Resp>(&self, hook: &str, payload: Req, default: Resp) -> Resp
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        match self.call(hook, payload).await {
            Ok(Some(result)) => result,
            Ok(None) => default,
            Err(e) => {
                tracing::warn!(hook, base_url = %self.base_url, error = %e, "module callback failed; using default");
                default
            }
        }
    }
}

// Request payload shapes. One struct per hook, named `<Hook>Payload`; kept in this module rather
// than `crate::callback` because they are this client's concrete choice of wire shape, not part
// of the protocol's envelope (a module author writing a server in another language only needs
// the envelope and these shapes' JSON, documented in `crate::callback`'s module doc comment).

#[derive(Debug, Serialize)]
struct UserMayInvitePayload<'a> {
    inviter: &'a str,
    invitee: &'a str,
    room_id: &'a str,
}

#[derive(Debug, Serialize)]
struct CheckUsernameForSpamPayload<'a> {
    profile: &'a UserProfile,
}

#[derive(Debug, Serialize)]
struct GetInterestedUsersPayload<'a> {
    presence_user_id: &'a str,
}

#[derive(Debug, Serialize)]
struct IsUserExpiredPayload<'a> {
    user_id: &'a str,
}

#[derive(Debug, Serialize)]
struct CheckPasswordPayload<'a> {
    user_id: &'a str,
    password: &'a str,
}

#[derive(Debug, Serialize)]
struct BackgroundUpdateGuidancePayload<'a> {
    update_name: &'a str,
}

#[derive(Debug, Serialize)]
struct OnAccountDataUpdatedPayload<'a> {
    user_id: &'a str,
    room_id: Option<&'a str>,
    account_data_type: &'a str,
    content: &'a serde_json::Value,
}

#[derive(Debug, Serialize)]
struct CheckMediaForSpamPayload<'a> {
    media: &'a MediaDescriptor,
}

#[derive(Debug, Serialize)]
struct RatelimitOverridePayload<'a> {
    user_id: &'a str,
    limiter: &'a str,
}

#[derive(Debug, Serialize)]
struct ShouldFederateRoomPayload<'a> {
    room_id: &'a str,
}

#[async_trait]
impl ModuleHooks for HttpCallbackClient {
    async fn check_event_for_spam(&self, event: &EventForCheck) -> CheckResult {
        self.call_or_default(hook_names::CHECK_EVENT_FOR_SPAM, event, CheckResult::Allow)
            .await
    }

    async fn user_may_invite(&self, inviter: &str, invitee: &str, room_id: &str) -> CheckResult {
        self.call_or_default(
            hook_names::USER_MAY_INVITE,
            UserMayInvitePayload {
                inviter,
                invitee,
                room_id,
            },
            CheckResult::Allow,
        )
        .await
    }

    async fn check_username_for_spam(&self, profile: &UserProfile) -> CheckResult {
        self.call_or_default(
            hook_names::CHECK_USERNAME_FOR_SPAM,
            CheckUsernameForSpamPayload { profile },
            CheckResult::Allow,
        )
        .await
    }

    async fn check_event_allowed(&self, event: &EventForCheck) -> RuleResult {
        self.call_or_default(hook_names::CHECK_EVENT_ALLOWED, event, RuleResult::Allow)
            .await
    }

    async fn get_interested_users(&self, presence_user_id: &str) -> PresenceInterest {
        self.call_or_default(
            hook_names::GET_INTERESTED_USERS,
            GetInterestedUsersPayload { presence_user_id },
            PresenceInterest::Users(Vec::new()),
        )
        .await
    }

    async fn is_user_expired(&self, user_id: &str) -> Option<i64> {
        self.call_or_default(
            hook_names::IS_USER_EXPIRED,
            IsUserExpiredPayload { user_id },
            None,
        )
        .await
    }

    async fn check_password(&self, user_id: &str, password: &str) -> Option<AuthResult> {
        self.call_or_default(
            hook_names::CHECK_PASSWORD,
            CheckPasswordPayload { user_id, password },
            None,
        )
        .await
    }

    async fn background_update_guidance(&self, update_name: &str) -> BackgroundUpdateGuidance {
        self.call_or_default(
            hook_names::BACKGROUND_UPDATE_GUIDANCE,
            BackgroundUpdateGuidancePayload { update_name },
            BackgroundUpdateGuidance::default(),
        )
        .await
    }

    async fn on_account_data_updated(
        &self,
        user_id: &str,
        room_id: Option<&str>,
        account_data_type: &str,
        content: &serde_json::Value,
    ) {
        // Fire-and-forget: there is no decision to fall back on, only a notification to relay.
        let payload = OnAccountDataUpdatedPayload {
            user_id,
            room_id,
            account_data_type,
            content,
        };
        if let Err(e) = self
            .call::<_, ()>(hook_names::ON_ACCOUNT_DATA_UPDATED, payload)
            .await
        {
            tracing::warn!(hook = hook_names::ON_ACCOUNT_DATA_UPDATED, base_url = %self.base_url, error = %e, "module notification failed");
        }
    }

    async fn check_media_for_spam(&self, media: &MediaDescriptor) -> CheckResult {
        self.call_or_default(
            hook_names::CHECK_MEDIA_FOR_SPAM,
            CheckMediaForSpamPayload { media },
            CheckResult::Allow,
        )
        .await
    }

    async fn ratelimit_override(&self, user_id: &str, limiter: &str) -> Option<RatelimitOverride> {
        self.call_or_default(
            hook_names::RATELIMIT_OVERRIDE,
            RatelimitOverridePayload { user_id, limiter },
            None,
        )
        .await
    }

    async fn should_federate_room(&self, room_id: &str) -> bool {
        self.call_or_default(
            hook_names::SHOULD_FEDERATE_ROOM,
            ShouldFederateRoomPayload { room_id },
            true,
        )
        .await
    }

    async fn extra_unsigned_fields(
        &self,
        event: &EventForCheck,
    ) -> HashMap<String, serde_json::Value> {
        self.call_or_default(hook_names::EXTRA_UNSIGNED_FIELDS, event, HashMap::new())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unreachable_module_falls_back_to_default() {
        // Nothing is listening on this port: every call should fail and fall back.
        let client =
            HttpCallbackClient::with_timeout("http://127.0.0.1:1", Duration::from_millis(200));
        let event = EventForCheck {
            event_id: "$1".into(),
            room_id: "!r:x".into(),
            sender: "@a:x".into(),
            event_type: "m.room.message".into(),
            state_key: None,
            content: serde_json::json!({}),
        };
        assert_eq!(
            client.check_event_for_spam(&event).await,
            CheckResult::Allow
        );
        assert!(client.should_federate_room("!r:x").await);
        assert_eq!(client.check_password("@a:x", "hunter2").await, None);
    }
}
