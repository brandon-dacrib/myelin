//! The module hook trait: the extension point every track calls instead of embedding
//! Synapse-shaped Python modules. Covers Synapse's eleven callback categories (see
//! `refs/synapse/docs/modules/` for behavior, read but never copied; AGPL-3.0).
//!
//! One representative method or two are modeled per category for Phase 0 (the brief's "day-one
//! work" is the trait shape and the callback protocol, not full Synapse callback parity, which is
//! explicitly a Phase 1/2 deliverable). Adding a method to [`ModuleHooks`] is additive and does
//! not need an RFC; removing or changing one's signature does.
//!
//! Every method takes `&self` and borrowed arguments and returns a decision type quickly:
//! modules run on the request path (most visibly `check_event_for_spam`), so a slow module is a
//! slow server. Implementations that call out over HTTP (`crate::client::HttpCallbackClient`)
//! are the caller's problem to time out, not this trait's.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A minimal view of a Matrix event, enough for spam and rule checks without depending on
/// `hs-model` (this crate has no dependency on the event/room tracks; see
/// `docs/workstreams/15-admin-api-and-modules.md`: modules are consumed by every track, so they
/// cannot depend back on any one of them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventForCheck {
    pub event_id: String,
    pub room_id: String,
    pub sender: String,
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_key: Option<String>,
    pub content: serde_json::Value,
}

/// A user profile, for spam checks that look at registration-time fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// A media upload, for the media-repository category.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaDescriptor {
    pub server_name: String,
    pub media_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub size_bytes: u64,
}

/// The result of a spam/permission check: Synapse's modules return `True`/`False`/`Codes.FOO`;
/// this is the same three-way decision spelled out instead of overloading a boolean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum CheckResult {
    Allow,
    Deny {
        #[serde(skip_serializing_if = "Option::is_none")]
        errcode: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl CheckResult {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            errcode: None,
            reason: Some(reason.into()),
        }
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// The result of a third-party-rules `check_event_allowed`: unlike a plain spam check, a rule can
/// rewrite the event's content instead of only allowing or denying it (Synapse:
/// `check_event_allowed` may return a modified event dict).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum RuleResult {
    Allow,
    Deny {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Replace {
        content: serde_json::Value,
    },
}

/// Which local users should receive a presence update for `presence_user_id` (Synapse's
/// presence router `get_interested_users`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresenceInterest {
    /// Every local user should see this update (Synapse's `PresenceRouter.ALL_USERS` sentinel).
    AllUsers,
    /// Only these local users should see it.
    Users(Vec<String>),
}

/// A password/token-based auth attempt's outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResult {
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// A per-user, per-limiter rate-limit override (Synapse: `get_ratelimit_override_for_user`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RatelimitOverride {
    pub messages_per_second: f64,
    pub burst_count: u32,
}

/// Guidance for a background-update batch (Synapse's background-update controller: how large a
/// batch should be, and how long to sleep between them).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BackgroundUpdateGuidance {
    pub batch_size: u64,
    pub sleep_ms: u64,
}

impl Default for BackgroundUpdateGuidance {
    fn default() -> Self {
        Self {
            batch_size: 100,
            sleep_ms: 0,
        }
    }
}

/// The hook trait every track calls. See the module doc comment for scope.
///
/// # Categories covered (Synapse's eleven)
/// 1. spam checker: [`check_event_for_spam`](ModuleHooks::check_event_for_spam), [`user_may_invite`](ModuleHooks::user_may_invite), [`check_username_for_spam`](ModuleHooks::check_username_for_spam)
/// 2. third-party rules: [`check_event_allowed`](ModuleHooks::check_event_allowed)
/// 3. presence router: [`get_interested_users`](ModuleHooks::get_interested_users)
/// 4. account validity: [`is_user_expired`](ModuleHooks::is_user_expired)
/// 5. password auth provider: [`check_password`](ModuleHooks::check_password)
/// 6. background-update controller: [`background_update_guidance`](ModuleHooks::background_update_guidance)
/// 7. account data: [`on_account_data_updated`](ModuleHooks::on_account_data_updated)
/// 8. media repository: [`check_media_for_spam`](ModuleHooks::check_media_for_spam)
/// 9. ratelimit: [`ratelimit_override`](ModuleHooks::ratelimit_override)
/// 10. federation: [`should_federate_room`](ModuleHooks::should_federate_room)
/// 11. add-extra-fields-to-unsigned: [`extra_unsigned_fields`](ModuleHooks::extra_unsigned_fields)
#[async_trait]
pub trait ModuleHooks: Send + Sync {
    /// Category 1 (spam checker). Called before a locally-created event is accepted.
    async fn check_event_for_spam(&self, event: &EventForCheck) -> CheckResult;

    /// Category 1 (spam checker). Called before an invite is sent.
    async fn user_may_invite(&self, inviter: &str, invitee: &str, room_id: &str) -> CheckResult;

    /// Category 1 (spam checker). Called at registration time.
    async fn check_username_for_spam(&self, profile: &UserProfile) -> CheckResult;

    /// Category 2 (third-party rules). Called for every event, local or remote, before it is
    /// persisted; may rewrite the event.
    async fn check_event_allowed(&self, event: &EventForCheck) -> RuleResult;

    /// Category 3 (presence router). Which local users should see `presence_user_id`'s updates.
    async fn get_interested_users(&self, presence_user_id: &str) -> PresenceInterest;

    /// Category 4 (account validity). `Some(expires_at_ms)` if the module tracks an expiry for
    /// this user; `None` defers to the server's own policy.
    async fn is_user_expired(&self, user_id: &str) -> Option<i64>;

    /// Category 5 (password auth provider). `None` means "this module has no opinion"; the
    /// server tries the next provider (or its own built-in check).
    async fn check_password(&self, user_id: &str, password: &str) -> Option<AuthResult>;

    /// Category 6 (background-update controller). How the given named update should batch.
    async fn background_update_guidance(&self, update_name: &str) -> BackgroundUpdateGuidance;

    /// Category 7 (account data). Notification only: a user's account data changed.
    /// `room_id` is `Some` for room account data, `None` for global.
    async fn on_account_data_updated(
        &self,
        user_id: &str,
        room_id: Option<&str>,
        account_data_type: &str,
        content: &serde_json::Value,
    );

    /// Category 8 (media repository). Called after upload, before the media is served.
    async fn check_media_for_spam(&self, media: &MediaDescriptor) -> CheckResult;

    /// Category 9 (ratelimit). `None` defers to the server's configured limits.
    async fn ratelimit_override(&self, user_id: &str, limiter: &str) -> Option<RatelimitOverride>;

    /// Category 10 (federation). Whether `room_id` may federate at all (independent of the
    /// room's own `m.federate` flag, which the server checks separately).
    async fn should_federate_room(&self, room_id: &str) -> bool;

    /// Category 11 (add-extra-fields-to-unsigned). Fields to merge into an outgoing event's
    /// `unsigned` object (for example annotating bridged events).
    async fn extra_unsigned_fields(
        &self,
        event: &EventForCheck,
    ) -> HashMap<String, serde_json::Value>;
}
