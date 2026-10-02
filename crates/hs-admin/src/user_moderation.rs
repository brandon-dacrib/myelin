//! The moderation and activity half of the Users area: suspension (MSC3823), shadow-bans,
//! per-user rate-limit overrides, a support session minted for a user (`users.login_as`),
//! a user's sessions, room memberships, statistics and uploads, and redacting everything a
//! user has sent (a task).
//!
//! Two source traits, because the facts live in two crates:
//!
//! - [`UserModerationSource`] is the account side: the flags and override on the account
//!   record, the sessions, and minting an access token. Implemented for real by `hs-auth`
//!   (`hs_auth::admin_moderation::AuthStoreUserModeration`).
//! - [`UserActivitySource`] is the room side: memberships, what the user has sent, and
//!   redacting it. Implemented for real by `hs-room` (`hs_room::admin_users`).
//!
//! What the flags *do* is enforced where the writes happen, not here: a suspended account is
//! refused `403 M_USER_SUSPENDED` by the room, profile and media write routes; a shadow-banned
//! account's messages and invitations are answered as if sent and never reach anyone; and a
//! rate-limit override replaces the message-sending limit for that one account. This module
//! only records the administrator's decision, audits it and announces it.
//!
//! Every write here appends an audit entry and publishes an event (RFC 0004 sections 9 and
//! 10); the two long-running operations (`users.redact_events`, `users.media.delete`) run on
//! [`crate::tasks::TaskRegistry::spawn`] with progress, and are cancellable through
//! `tasks.cancel`.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hs_http::{Problem, ValidationError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::handler_kit::{authorize, check_replay, record, respond_and_remember, unwired};
use crate::media::AdminMediaItem;
use crate::model::{AdminUser, AuditChange, Page, ResourceRef, Scope};
use crate::router::{AdminState, parse_optional_json};
use crate::sources::SourceError;

// -------------------------------------------------------------------------------------------
// wire shapes
// -------------------------------------------------------------------------------------------

/// The OpenAPI `RateLimitOverride` schema: how fast one user may send events, in place of the
/// server's `rate_limits.message` bucket. `messages_per_second: 0` exempts the user entirely
/// (Synapse's convention for the same override). An empty object from `GET` means no override
/// is set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitOverride {
    /// Sustained rate, events per second. `0` means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_per_second: Option<f64>,
    /// How many events may be sent back to back before the sustained rate applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst_count: Option<u32>,
}

/// The burst a `PUT` without `burst_count` gets.
pub const DEFAULT_OVERRIDE_BURST: u32 = 10;

/// The OpenAPI `Session` schema: one signed-in device, as the server last saw it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminSession {
    pub device_id: String,
    /// The device's display name, as the client set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// When the session began, if the server recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub last_seen_at: Option<String>,
    /// Whether this session is a support session an administrator minted with
    /// `users.login_as`.
    #[serde(default)]
    pub support_session: bool,
}

/// One room a user has a membership in: the OpenAPI `RoomMember` schema, with the room it is
/// in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminUserMembership {
    pub room_id: String,
    /// The room's name, if it has one.
    pub room_name: Option<String>,
    pub user_id: String,
    /// `join`, `invite`, `leave`, `ban` or `knock`.
    pub membership: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

/// What the room side counts about one user, for `users.statistics.get`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserActivityCounts {
    /// Rooms the user is joined to now.
    pub joins_count: u64,
    /// Invitations the user has sent.
    pub invites_sent_count: u64,
    /// Events the user has sent, state and membership included.
    pub events_sent_count: u64,
    /// Rooms the user created.
    pub rooms_created_count: u64,
}

/// `GET /users/{user_id}/statistics`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminUserStatistics {
    pub user_id: String,
    pub joins_count: u64,
    pub invites_sent_count: u64,
    pub events_sent_count: u64,
    pub rooms_created_count: u64,
    /// `None` when no media repository is wired.
    pub media_count: Option<u64>,
    pub media_bytes: Option<u64>,
    /// `None` when the account side is not wired.
    pub session_count: Option<u64>,
}

/// The answer to `users.login_as`: an access token acting as the user. Debug never shows it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SupportToken {
    pub user_id: String,
    pub access_token: String,
    pub device_id: String,
    /// When the token stops working, RFC 3339.
    pub expires_at: String,
}

impl std::fmt::Debug for SupportToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupportToken")
            .field("user_id", &self.user_id)
            .field("access_token", &"<redacted>")
            .field("device_id", &self.device_id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// One event `users.redact_events` will redact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactTarget {
    pub room_id: String,
    pub event_id: String,
}

// -------------------------------------------------------------------------------------------
// sources
// -------------------------------------------------------------------------------------------

/// The account side of user moderation. Every method answers [`SourceError::NotFound`] for a
/// user that does not exist.
#[async_trait]
pub trait UserModerationSource: Send + Sync + 'static {
    /// Suspends the account (MSC3823: it can read, not write) or lifts the suspension.
    async fn set_suspended(&self, user_id: &str, suspended: bool) -> Result<(), SourceError>;
    /// Shadow-bans the account (its messages and invitations reach nobody, and it is not told)
    /// or lifts the ban.
    async fn set_shadow_banned(
        &self,
        user_id: &str,
        shadow_banned: bool,
    ) -> Result<(), SourceError>;
    /// The account's rate-limit override, if one is set.
    async fn rate_limit(&self, user_id: &str) -> Result<Option<RateLimitOverride>, SourceError>;
    /// Sets (`Some`) or clears (`None`) the account's rate-limit override. A `Some` has both
    /// fields filled in.
    async fn set_rate_limit(
        &self,
        user_id: &str,
        rate_limit: Option<RateLimitOverride>,
    ) -> Result<(), SourceError>;
    /// The account's sessions, most recently seen first.
    async fn sessions(&self, user_id: &str) -> Result<Vec<AdminSession>, SourceError>;
    /// Mints a new session (a device and an access token) acting as the user, valid for
    /// `valid_for_ms`. `minted_by` names the administrator, for the device's display name.
    async fn mint_support_token(
        &self,
        user_id: &str,
        valid_for_ms: u64,
        minted_by: &str,
    ) -> Result<SupportToken, SourceError>;
}

/// The room side of a user's activity.
#[async_trait]
pub trait UserActivitySource: Send + Sync + 'static {
    /// Every room in which the user has a membership event in the current state.
    async fn memberships(&self, user_id: &str) -> Result<Vec<AdminUserMembership>, SourceError>;
    /// Counts of what the user has done.
    async fn statistics(&self, user_id: &str) -> Result<UserActivityCounts, SourceError>;
    /// The events of the user's that `users.redact_events` would redact: messages and other
    /// non-state events not already redacted (redactions themselves excluded), newest first,
    /// in `room_id` only if given, at most `limit` if given. [`SourceError::NotFound`] if
    /// `room_id` names no room.
    async fn events_to_redact(
        &self,
        user_id: &str,
        room_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<RedactTarget>, SourceError>;
    /// Redacts one event, as whoever in the room may.
    async fn redact_event(
        &self,
        target: &RedactTarget,
        reason: Option<&str>,
    ) -> Result<(), SourceError>;
    /// Leaves every room the user is joined to, invited to or knocking on, as the user
    /// themself, the way `POST /rooms/{id}/leave` does (through another server for a room no
    /// user of this server is joined to). A room that cannot be left is reported in
    /// [`LeaveReport::rooms_failed`], not an error: the caller (`users.deactivate` with
    /// `erase: true`) goes on to erase the account either way. `Err` only when nothing could be
    /// attempted (an invalid user id, the room store unavailable).
    async fn leave_all_rooms(&self, user_id: &str) -> Result<LeaveReport, SourceError> {
        let _ = user_id;
        Err(SourceError::Unavailable(
            "this activity source cannot leave rooms yet".to_string(),
        ))
    }
}

/// What [`UserActivitySource::leave_all_rooms`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaveReport {
    /// The rooms left, by id.
    pub rooms_left: Vec<String>,
    /// The rooms the user is still in, with why each leave failed.
    pub rooms_failed: Vec<LeaveFailure>,
}

/// One room [`UserActivitySource::leave_all_rooms`] could not leave.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaveFailure {
    /// The room.
    pub room_id: String,
    /// The room layer's reason, as text.
    pub reason: String,
}

/// An in-memory [`UserModerationSource`], for tests and the mock server.
#[derive(Debug, Default)]
pub struct InMemoryUserModeration {
    users: RwLock<BTreeMap<String, ModerationRecord>>,
}

#[derive(Debug, Default, Clone)]
struct ModerationRecord {
    suspended: bool,
    shadow_banned: bool,
    rate_limit: Option<RateLimitOverride>,
    sessions: Vec<AdminSession>,
}

impl InMemoryUserModeration {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a user with the given sessions.
    #[must_use]
    pub fn with_user(self, user_id: &str, sessions: Vec<AdminSession>) -> Self {
        self.write().insert(
            user_id.to_owned(),
            ModerationRecord {
                sessions,
                ..ModerationRecord::default()
            },
        );
        self
    }

    /// Whether the user is suspended here.
    #[must_use]
    pub fn is_suspended(&self, user_id: &str) -> bool {
        self.read().get(user_id).is_some_and(|r| r.suspended)
    }

    /// Whether the user is shadow-banned here.
    #[must_use]
    pub fn is_shadow_banned(&self, user_id: &str) -> bool {
        self.read().get(user_id).is_some_and(|r| r.shadow_banned)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, ModerationRecord>> {
        self.users
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, ModerationRecord>> {
        self.users
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn change(
        &self,
        user_id: &str,
        f: impl FnOnce(&mut ModerationRecord),
    ) -> Result<(), SourceError> {
        let mut users = self.write();
        let record = users.get_mut(user_id).ok_or(SourceError::NotFound)?;
        f(record);
        Ok(())
    }
}

#[async_trait]
impl UserModerationSource for InMemoryUserModeration {
    async fn set_suspended(&self, user_id: &str, suspended: bool) -> Result<(), SourceError> {
        self.change(user_id, |r| r.suspended = suspended)
    }

    async fn set_shadow_banned(
        &self,
        user_id: &str,
        shadow_banned: bool,
    ) -> Result<(), SourceError> {
        self.change(user_id, |r| r.shadow_banned = shadow_banned)
    }

    async fn rate_limit(&self, user_id: &str) -> Result<Option<RateLimitOverride>, SourceError> {
        self.read()
            .get(user_id)
            .map(|r| r.rate_limit)
            .ok_or(SourceError::NotFound)
    }

    async fn set_rate_limit(
        &self,
        user_id: &str,
        rate_limit: Option<RateLimitOverride>,
    ) -> Result<(), SourceError> {
        self.change(user_id, |r| r.rate_limit = rate_limit)
    }

    async fn sessions(&self, user_id: &str) -> Result<Vec<AdminSession>, SourceError> {
        self.read()
            .get(user_id)
            .map(|r| r.sessions.clone())
            .ok_or(SourceError::NotFound)
    }

    async fn mint_support_token(
        &self,
        user_id: &str,
        valid_for_ms: u64,
        minted_by: &str,
    ) -> Result<SupportToken, SourceError> {
        let device_id = format!("SUPPORT{}", crate::model::new_id());
        let expires_at = time::OffsetDateTime::now_utc()
            + time::Duration::milliseconds(i64::try_from(valid_for_ms).unwrap_or(i64::MAX));
        let token = SupportToken {
            user_id: user_id.to_owned(),
            access_token: format!("syt_support_{}", crate::model::new_id()),
            device_id: device_id.clone(),
            expires_at: hs_http::time::format_rfc3339(expires_at),
        };
        self.change(user_id, |r| {
            r.sessions.push(AdminSession {
                device_id,
                display_name: Some(format!("Support session for {minted_by}")),
                support_session: true,
                ..AdminSession::default()
            });
        })?;
        Ok(token)
    }
}

/// An in-memory [`UserActivitySource`], for tests and the mock server: memberships and the
/// user's events are given, redacting one removes it from what is left to redact.
#[derive(Debug, Default)]
pub struct InMemoryUserActivity {
    memberships: RwLock<Vec<AdminUserMembership>>,
    events: RwLock<Vec<(String, RedactTarget)>>,
    redacted: RwLock<Vec<RedactTarget>>,
    /// Rooms `leave_all_rooms` reports as failed instead of leaving.
    unleavable: RwLock<Vec<String>>,
}

impl InMemoryUserActivity {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_membership(self, membership: AdminUserMembership) -> Self {
        self.memberships
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(membership);
        self
    }

    /// An event `user_id` sent.
    #[must_use]
    pub fn with_event(self, user_id: &str, room_id: &str, event_id: &str) -> Self {
        self.events
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                user_id.to_owned(),
                RedactTarget {
                    room_id: room_id.to_owned(),
                    event_id: event_id.to_owned(),
                },
            ));
        self
    }

    /// Makes `leave_all_rooms` fail for `room_id`, reporting it rather than leaving it.
    #[must_use]
    pub fn with_unleavable_room(self, room_id: &str) -> Self {
        self.unleavable
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(room_id.to_owned());
        self
    }

    /// Everything redacted so far, in order.
    #[must_use]
    pub fn redacted(&self) -> Vec<RedactTarget> {
        self.redacted
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl UserActivitySource for InMemoryUserActivity {
    async fn memberships(&self, user_id: &str) -> Result<Vec<AdminUserMembership>, SourceError> {
        Ok(self
            .memberships
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|m| m.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn statistics(&self, user_id: &str) -> Result<UserActivityCounts, SourceError> {
        let memberships = self.memberships(user_id).await?;
        let events = self
            .events
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(u, _)| u == user_id)
            .count() as u64;
        Ok(UserActivityCounts {
            joins_count: memberships
                .iter()
                .filter(|m| m.membership == "join")
                .count() as u64,
            events_sent_count: events,
            ..UserActivityCounts::default()
        })
    }

    async fn events_to_redact(
        &self,
        user_id: &str,
        room_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<RedactTarget>, SourceError> {
        let redacted = self.redacted();
        let mut out: Vec<RedactTarget> = self
            .events
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .filter(|(u, t)| {
                u == user_id && room_id.is_none_or(|r| t.room_id == r) && !redacted.contains(t)
            })
            .map(|(_, t)| t.clone())
            .collect();
        if let Some(limit) = limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn redact_event(
        &self,
        target: &RedactTarget,
        _reason: Option<&str>,
    ) -> Result<(), SourceError> {
        self.redacted
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(target.clone());
        Ok(())
    }

    async fn leave_all_rooms(&self, user_id: &str) -> Result<LeaveReport, SourceError> {
        let unleavable = self
            .unleavable
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut report = LeaveReport::default();
        let mut memberships = self
            .memberships
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for m in memberships.iter_mut().filter(|m| {
            m.user_id == user_id && matches!(m.membership.as_str(), "join" | "invite" | "knock")
        }) {
            if unleavable.contains(&m.room_id) {
                report.rooms_failed.push(LeaveFailure {
                    room_id: m.room_id.clone(),
                    reason: "cannot be left in this test".to_owned(),
                });
            } else {
                m.membership = "leave".to_owned();
                report.rooms_left.push(m.room_id.clone());
            }
        }
        Ok(report)
    }
}

// -------------------------------------------------------------------------------------------
// helpers
// -------------------------------------------------------------------------------------------

fn user_instance(user_id: &str, suffix: &str) -> String {
    format!("/api/v1/users/{user_id}/{suffix}")
}

#[allow(clippy::result_large_err)]
fn moderation_source(
    state: &AdminState,
    instance: &str,
) -> Result<Arc<dyn UserModerationSource>, Response> {
    state
        .user_moderation
        .clone()
        .ok_or_else(|| unwired("user moderation", instance))
}

#[allow(clippy::result_large_err)]
fn activity_source(
    state: &AdminState,
    instance: &str,
) -> Result<Arc<dyn UserActivitySource>, Response> {
    state
        .user_activity
        .clone()
        .ok_or_else(|| unwired("user activity", instance))
}

fn source_problem(e: &SourceError, user_id: &str, instance: &str) -> Response {
    match e {
        SourceError::NotFound => Problem::not_found()
            .with_detail(format!("no such user: {user_id}"))
            .with_instance(instance.to_owned())
            .into_response(),
        other => other
            .to_problem()
            .with_instance(instance.to_owned())
            .into_response(),
    }
}

/// The user, from the user directory: `404` if there is none, `503` if no directory is wired.
#[allow(clippy::result_large_err)]
async fn fetch_user(
    state: &AdminState,
    user_id: &str,
    instance: &str,
) -> Result<AdminUser, Response> {
    let Some(users) = &state.users else {
        return Err(unwired("user directory", instance));
    };
    match users.get_user(user_id).await {
        Ok(Some(user)) => Ok(user),
        Ok(None) => Err(source_problem(&SourceError::NotFound, user_id, instance)),
        Err(e) => Err(source_problem(&e, user_id, instance)),
    }
}

fn invalid(pointer: &str, detail: impl Into<String>) -> Problem {
    let detail = detail.into();
    Problem::validation_failed()
        .with_detail(detail.clone())
        .with_errors(vec![ValidationError::new(pointer, detail)])
}

/// The body `users.suspend` and its siblings accept.
#[derive(Debug, Default, Deserialize)]
struct ReasonBody {
    reason: Option<String>,
}

// -------------------------------------------------------------------------------------------
// suspend, unsuspend, shadow-ban, unshadow-ban
// -------------------------------------------------------------------------------------------

/// Which flag a toggle changes.
#[derive(Debug, Clone, Copy)]
enum Flag {
    Suspended,
    ShadowBanned,
}

impl Flag {
    fn pointer(self) -> &'static str {
        match self {
            Flag::Suspended => "/suspended",
            Flag::ShadowBanned => "/shadow_banned",
        }
    }

    fn current(self, user: &AdminUser) -> bool {
        match self {
            Flag::Suspended => user.suspended,
            Flag::ShadowBanned => user.shadow_banned,
        }
    }

    fn names(self, on: bool) -> (&'static str, &'static str, &'static str) {
        match (self, on) {
            (Flag::Suspended, true) => ("users.suspend", "user.suspended", "suspend"),
            (Flag::Suspended, false) => ("users.unsuspend", "user.unsuspended", "unsuspend"),
            (Flag::ShadowBanned, true) => ("users.shadow_ban", "user.shadow_banned", "shadow-ban"),
            (Flag::ShadowBanned, false) => {
                ("users.unshadow_ban", "user.unshadow_banned", "unshadow-ban")
            }
        }
    }

    fn scope(self) -> Scope {
        // RFC 0004 section 8: both are a moderator's tools, like locking.
        match self {
            Flag::Suspended | Flag::ShadowBanned => Scope::ModerationWrite,
        }
    }
}

async fn toggle(
    state: AdminState,
    headers: HeaderMap,
    user_id: String,
    body: axum::body::Bytes,
    flag: Flag,
    on: bool,
) -> Response {
    let (operation_id, event_type, suffix) = flag.names(on);
    let instance = user_instance(&user_id, suffix);
    let principal = match authorize(&state, &headers, flag.scope(), &instance).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let request: ReasonBody = match parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    if let Err(response) = check_replay(&state, &headers, operation_id, &body, &instance) {
        return response;
    }
    let moderation = match moderation_source(&state, &instance) {
        Ok(m) => m,
        Err(response) => return response,
    };
    let before = match fetch_user(&state, &user_id, &instance).await {
        Ok(u) => u,
        Err(response) => return response,
    };
    let changed = flag.current(&before) != on;
    // Set even when the directory already says so: setting is idempotent, and the account
    // store, not the directory's view of it, is what the write routes enforce.
    let result = match flag {
        Flag::Suspended => moderation.set_suspended(&user_id, on).await,
        Flag::ShadowBanned => moderation.set_shadow_banned(&user_id, on).await,
    };
    if let Err(e) = result {
        return source_problem(&e, &user_id, &instance);
    }
    let after = match fetch_user(&state, &user_id, &instance).await {
        Ok(u) => u,
        Err(response) => return response,
    };
    let changes = if changed {
        vec![AuditChange {
            pointer: flag.pointer().to_owned(),
            from: Some(json!(!on)),
            to: Some(json!(on)),
        }]
    } else {
        Vec::new()
    };
    tracing::info!(
        target: "hs_admin::users",
        user = %user_id,
        by = %principal.id,
        operation = operation_id,
        changed,
        reason = request.reason.as_deref().unwrap_or(""),
        "user moderation flag set"
    );
    let data = match &request.reason {
        Some(reason) => json!({ "reason": reason }),
        None => json!({}),
    };
    if let Err(response) = record(
        &state,
        &principal,
        operation_id,
        event_type,
        ResourceRef::new("user", user_id.clone()),
        changes,
        data,
        200,
    )
    .await
    {
        return response;
    }
    respond_and_remember(
        &state,
        &headers,
        operation_id,
        &body,
        StatusCode::OK,
        &after,
        &[],
    )
}

/// `POST /api/v1/users/{user_id}/suspend` (`moderation:write`): MSC3823. The user keeps reading
/// and can still leave rooms, redact their own messages and sign out; every other write is
/// refused with `403 M_USER_SUSPENDED`.
pub(crate) async fn users_suspend(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    toggle(state, headers, user_id, body, Flag::Suspended, true).await
}

/// `POST /api/v1/users/{user_id}/unsuspend` (`moderation:write`).
pub(crate) async fn users_unsuspend(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    toggle(state, headers, user_id, body, Flag::Suspended, false).await
}

/// `POST /api/v1/users/{user_id}/shadow-ban` (`moderation:write`): the user's messages and
/// invitations are answered as if sent and reach nobody.
pub(crate) async fn users_shadow_ban(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    toggle(state, headers, user_id, body, Flag::ShadowBanned, true).await
}

/// `POST /api/v1/users/{user_id}/unshadow-ban` (`moderation:write`).
pub(crate) async fn users_unshadow_ban(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    toggle(state, headers, user_id, body, Flag::ShadowBanned, false).await
}

// -------------------------------------------------------------------------------------------
// rate limit
// -------------------------------------------------------------------------------------------

/// `GET /api/v1/users/{user_id}/rate-limit` (`admin:read`): the override, or `{}` for none.
pub(crate) async fn users_rate_limit_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let instance = user_instance(&user_id, "rate-limit");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    let moderation = match moderation_source(&state, &instance) {
        Ok(m) => m,
        Err(response) => return response,
    };
    match moderation.rate_limit(&user_id).await {
        Ok(value) => axum::Json(value.unwrap_or_default()).into_response(),
        Err(e) => source_problem(&e, &user_id, &instance),
    }
}

/// Checks and completes a `PUT` body: `messages_per_second` is required, finite and not
/// negative; `burst_count` is at least 1 and defaults to [`DEFAULT_OVERRIDE_BURST`].
#[allow(clippy::result_large_err)]
fn complete_override(request: RateLimitOverride) -> Result<RateLimitOverride, Problem> {
    let Some(per_second) = request.messages_per_second else {
        return Err(invalid(
            "/messages_per_second",
            "messages_per_second is required (0 exempts the user from the limit)",
        ));
    };
    if !per_second.is_finite() || per_second < 0.0 {
        return Err(invalid(
            "/messages_per_second",
            format!("messages_per_second must be a number of at least 0, not {per_second}"),
        ));
    }
    let burst = request.burst_count.unwrap_or(DEFAULT_OVERRIDE_BURST);
    if burst == 0 {
        return Err(invalid(
            "/burst_count",
            "burst_count must be at least 1: a burst of 0 would refuse every message",
        ));
    }
    Ok(RateLimitOverride {
        messages_per_second: Some(per_second),
        burst_count: Some(burst),
    })
}

/// `PUT /api/v1/users/{user_id}/rate-limit` (`admin:write`).
pub(crate) async fn users_rate_limit_put(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = user_instance(&user_id, "rate-limit");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let request: RateLimitOverride = match parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let wanted = match complete_override(request) {
        Ok(v) => v,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let moderation = match moderation_source(&state, &instance) {
        Ok(m) => m,
        Err(response) => return response,
    };
    let before = match moderation.rate_limit(&user_id).await {
        Ok(v) => v,
        Err(e) => return source_problem(&e, &user_id, &instance),
    };
    if let Err(e) = moderation.set_rate_limit(&user_id, Some(wanted)).await {
        return source_problem(&e, &user_id, &instance);
    }
    tracing::info!(
        target: "hs_admin::users",
        user = %user_id,
        by = %principal.id,
        messages_per_second = wanted.messages_per_second.unwrap_or_default(),
        burst_count = wanted.burst_count.unwrap_or_default(),
        "rate-limit override set"
    );
    let changes = vec![AuditChange {
        pointer: "/rate_limit".to_owned(),
        from: before.map(|b| json!(b)),
        to: Some(json!(wanted)),
    }];
    if let Err(response) = record(
        &state,
        &principal,
        "users.rate_limit.put",
        "user.rate_limit_changed",
        ResourceRef::new("user", user_id.clone()),
        changes,
        json!({ "rate_limit": wanted }),
        200,
    )
    .await
    {
        return response;
    }
    axum::Json(wanted).into_response()
}

/// `DELETE /api/v1/users/{user_id}/rate-limit` (`admin:write`): back to the server's limit.
pub(crate) async fn users_rate_limit_delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let instance = user_instance(&user_id, "rate-limit");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let moderation = match moderation_source(&state, &instance) {
        Ok(m) => m,
        Err(response) => return response,
    };
    let before = match moderation.rate_limit(&user_id).await {
        Ok(v) => v,
        Err(e) => return source_problem(&e, &user_id, &instance),
    };
    if before.is_some()
        && let Err(e) = moderation.set_rate_limit(&user_id, None).await
    {
        return source_problem(&e, &user_id, &instance);
    }
    tracing::info!(target: "hs_admin::users", user = %user_id, by = %principal.id, "rate-limit override cleared");
    let changes = before
        .map(|b| {
            vec![AuditChange {
                pointer: "/rate_limit".to_owned(),
                from: Some(json!(b)),
                to: None,
            }]
        })
        .unwrap_or_default();
    if let Err(response) = record(
        &state,
        &principal,
        "users.rate_limit.delete",
        "user.rate_limit_changed",
        ResourceRef::new("user", user_id.clone()),
        changes,
        json!({ "rate_limit": null }),
        204,
    )
    .await
    {
        return response;
    }
    StatusCode::NO_CONTENT.into_response()
}

// -------------------------------------------------------------------------------------------
// login as
// -------------------------------------------------------------------------------------------

/// How long a support session lasts when the request does not say.
pub const DEFAULT_SUPPORT_SESSION_SECONDS: u64 = 3600;
/// The longest support session that can be asked for: a day.
pub const MAX_SUPPORT_SESSION_SECONDS: u64 = 86_400;

#[derive(Debug, Default, Deserialize)]
struct LoginAsBody {
    reason: Option<String>,
    valid_for_seconds: Option<u64>,
}

/// `POST /api/v1/users/{user_id}/login-as` (`admin:write`): a fresh session acting as the user,
/// for support. The token is in this answer and nowhere else: not in the audit entry, not in
/// the event, not in the log. What *is* recorded, loudly, is that it was minted, by whom, for
/// how long and why, at `warn` level in the log and as a `user.impersonated` event.
///
/// Refused `403` unless the caller holds `admin:write` itself (not through any other scope),
/// and `409` for a deactivated account or one the caller is signed in as. The session shows
/// among the user's sessions as a support session and can be signed out like any other.
pub(crate) async fn users_login_as(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = user_instance(&user_id, "login-as");
    let principal = match authorize(&state, &headers, Scope::AdminWrite, &instance).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    if !principal.scopes.contains(&Scope::AdminWrite) {
        return Problem::insufficient_scope()
            .with_detail("minting a session as another user needs the admin:write scope itself")
            .with_instance(instance)
            .into_response();
    }
    let request: LoginAsBody = match parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    let seconds = request
        .valid_for_seconds
        .unwrap_or(DEFAULT_SUPPORT_SESSION_SECONDS);
    if seconds == 0 || seconds > MAX_SUPPORT_SESSION_SECONDS {
        return invalid(
            "/valid_for_seconds",
            format!(
                "valid_for_seconds must be between 1 and {MAX_SUPPORT_SESSION_SECONDS}, not {seconds}"
            ),
        )
        .with_instance(instance)
        .into_response();
    }
    if let Err(response) = check_replay(&state, &headers, "users.login_as", &body, &instance) {
        return response;
    }
    let moderation = match moderation_source(&state, &instance) {
        Ok(m) => m,
        Err(response) => return response,
    };
    let user = match fetch_user(&state, &user_id, &instance).await {
        Ok(u) => u,
        Err(response) => return response,
    };
    if user.deactivated {
        return Problem::conflict()
            .with_detail(format!("{user_id} is deactivated; nobody can act as it"))
            .with_instance(instance)
            .into_response();
    }
    if user.user_id == principal.id {
        return Problem::conflict()
            .with_detail("you are already signed in as this user")
            .with_instance(instance)
            .into_response();
    }
    let token = match moderation
        .mint_support_token(&user_id, seconds * 1000, &principal.id)
        .await
    {
        Ok(t) => t,
        Err(e) => return source_problem(&e, &user_id, &instance),
    };
    tracing::warn!(
        target: "hs_admin::users",
        user = %user_id,
        by = %principal.id,
        device = %token.device_id,
        expires_at = %token.expires_at,
        reason = request.reason.as_deref().unwrap_or(""),
        "an administrator minted a support session acting as a user"
    );
    let changes = vec![AuditChange {
        pointer: "/sessions/-".to_owned(),
        from: None,
        to: Some(json!({
            "device_id": token.device_id,
            "expires_at": token.expires_at,
            "support_session": true,
        })),
    }];
    if let Err(response) = record(
        &state,
        &principal,
        "users.login_as",
        "user.impersonated",
        ResourceRef::new("user", user_id.clone()),
        changes,
        json!({
            "device_id": token.device_id,
            "expires_at": token.expires_at,
            "valid_for_seconds": seconds,
            "reason": request.reason,
        }),
        201,
    )
    .await
    {
        return response;
    }
    respond_and_remember(
        &state,
        &headers,
        "users.login_as",
        &body,
        StatusCode::CREATED,
        &token,
        &[],
    )
}

// -------------------------------------------------------------------------------------------
// reads: sessions, memberships, statistics, media
// -------------------------------------------------------------------------------------------

/// `?limit=&cursor=&include_total=` (and `membership=` for memberships).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct UserPageQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    include_total: Option<bool>,
    membership: Option<String>,
}

fn page<T>(items: Vec<T>, query: &UserPageQuery) -> Page<T> {
    Page::paginate(
        items,
        query.cursor.as_deref(),
        query.limit,
        query.include_total.unwrap_or(false),
    )
}

/// `GET /api/v1/users/{user_id}/sessions` (`admin:read`): the user's sessions, most recently
/// seen first, with the address each was last seen from.
pub(crate) async fn users_sessions_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Query(query): Query<UserPageQuery>,
) -> Response {
    let instance = user_instance(&user_id, "sessions");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    let moderation = match moderation_source(&state, &instance) {
        Ok(m) => m,
        Err(response) => return response,
    };
    match moderation.sessions(&user_id).await {
        Ok(sessions) => axum::Json(page(sessions, &query)).into_response(),
        Err(e) => source_problem(&e, &user_id, &instance),
    }
}

const MEMBERSHIPS: &[&str] = &["join", "invite", "leave", "ban", "knock"];

/// `GET /api/v1/users/{user_id}/memberships` (`admin:read`): every room the user has a
/// membership in, joined rooms first; `?membership=` narrows to one value.
pub(crate) async fn users_memberships_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Query(query): Query<UserPageQuery>,
) -> Response {
    let instance = user_instance(&user_id, "memberships");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    if let Some(m) = query.membership.as_deref()
        && !MEMBERSHIPS.contains(&m)
    {
        return invalid(
            "/membership",
            format!(
                "membership must be one of {}, not {m:?}",
                MEMBERSHIPS.join(", ")
            ),
        )
        .with_instance(instance)
        .into_response();
    }
    let activity = match activity_source(&state, &instance) {
        Ok(a) => a,
        Err(response) => return response,
    };
    if let Err(response) = fetch_user(&state, &user_id, &instance).await {
        return response;
    }
    let mut memberships = match activity.memberships(&user_id).await {
        Ok(m) => m,
        Err(e) => return source_problem(&e, &user_id, &instance),
    };
    if let Some(wanted) = query.membership.as_deref() {
        memberships.retain(|m| m.membership == wanted);
    }
    memberships.sort_by(|a, b| {
        let rank = |m: &str| MEMBERSHIPS.iter().position(|x| *x == m).unwrap_or(9);
        rank(&a.membership)
            .cmp(&rank(&b.membership))
            .then_with(|| a.room_id.cmp(&b.room_id))
    });
    axum::Json(page(memberships, &query)).into_response()
}

/// The user's local uploads, newest first; `None` when no media repository is wired.
async fn uploads_of(
    state: &AdminState,
    user_id: &str,
) -> Result<Option<Vec<AdminMediaItem>>, SourceError> {
    let Some(media) = &state.media else {
        return Ok(None);
    };
    let mut items: Vec<AdminMediaItem> = media
        .list()
        .await?
        .into_iter()
        .filter(|m| m.origin == "local" && m.uploader.as_deref() == Some(user_id))
        .collect();
    items.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.media_id.cmp(&b.media_id))
    });
    Ok(Some(items))
}

/// `GET /api/v1/users/{user_id}/statistics` (`admin:read`).
pub(crate) async fn users_statistics_get(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let instance = user_instance(&user_id, "statistics");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    let activity = match activity_source(&state, &instance) {
        Ok(a) => a,
        Err(response) => return response,
    };
    if let Err(response) = fetch_user(&state, &user_id, &instance).await {
        return response;
    }
    let counts = match activity.statistics(&user_id).await {
        Ok(c) => c,
        Err(e) => return source_problem(&e, &user_id, &instance),
    };
    let uploads = match uploads_of(&state, &user_id).await {
        Ok(u) => u,
        Err(e) => return source_problem(&e, &user_id, &instance),
    };
    let session_count = match &state.user_moderation {
        Some(m) => match m.sessions(&user_id).await {
            Ok(s) => Some(s.len() as u64),
            Err(e) => return source_problem(&e, &user_id, &instance),
        },
        None => None,
    };
    axum::Json(AdminUserStatistics {
        user_id: user_id.clone(),
        joins_count: counts.joins_count,
        invites_sent_count: counts.invites_sent_count,
        events_sent_count: counts.events_sent_count,
        rooms_created_count: counts.rooms_created_count,
        media_count: uploads.as_ref().map(|u| u.len() as u64),
        media_bytes: uploads
            .as_ref()
            .map(|u| u.iter().map(|m| m.size_bytes).sum()),
        session_count,
    })
    .into_response()
}

/// `GET /api/v1/users/{user_id}/media` (`admin:read`): what the user uploaded, newest first.
pub(crate) async fn users_media_list(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Query(query): Query<UserPageQuery>,
) -> Response {
    let instance = user_instance(&user_id, "media");
    if let Err(response) = authorize(&state, &headers, Scope::AdminRead, &instance).await {
        return response;
    }
    if state.media.is_none() {
        return unwired("media repository", &instance);
    }
    if let Err(response) = fetch_user(&state, &user_id, &instance).await {
        return response;
    }
    match uploads_of(&state, &user_id).await {
        Ok(items) => axum::Json(page(items.unwrap_or_default(), &query)).into_response(),
        Err(e) => source_problem(&e, &user_id, &instance),
    }
}

// -------------------------------------------------------------------------------------------
// the two tasks
// -------------------------------------------------------------------------------------------

/// Answers a just-spawned task the way every task-starting operation does: `202`, the task,
/// and its `Location`.
fn accepted(
    state: &AdminState,
    headers: &HeaderMap,
    operation_id: &str,
    body: &[u8],
    task: &crate::model::Task,
) -> Response {
    respond_and_remember(
        state,
        headers,
        operation_id,
        body,
        StatusCode::ACCEPTED,
        task,
        &[("location", format!("/api/v1/tasks/{}", task.id))],
    )
}

/// `DELETE /api/v1/users/{user_id}/media` (`moderation:write`, Task): deletes everything the
/// user uploaded, bytes, thumbnails and all, except protected items (protection exists to
/// guard against exactly this kind of sweep; unprotect one first to delete it). Progress is
/// counted in items; the result says how many went, how many bytes, and what was kept.
pub(crate) async fn users_media_delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Response {
    let instance = user_instance(&user_id, "media");
    let principal = match authorize(&state, &headers, Scope::ModerationWrite, &instance).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let Some(media) = state.media.clone() else {
        return unwired("media repository", &instance);
    };
    let Some(tasks) = state.tasks.clone() else {
        return unwired("task registry", &instance);
    };
    if let Err(response) = fetch_user(&state, &user_id, &instance).await {
        return response;
    }
    let items = match uploads_of(&state, &user_id).await {
        Ok(items) => items.unwrap_or_default(),
        Err(e) => return source_problem(&e, &user_id, &instance),
    };
    if let Err(response) = record(
        &state,
        &principal,
        "users.media.delete",
        "user.media_deletion_started",
        ResourceRef::new("user", user_id.clone()),
        Vec::new(),
        json!({ "count": items.len() }),
        202,
    )
    .await
    {
        return response;
    }
    let events = state.events.clone();
    let actor = principal.to_actor();
    let owner = user_id.clone();
    let spawned = tasks
        .spawn(
            "media.delete",
            Some(ResourceRef::new("user", user_id.clone())),
            principal.to_actor(),
            move |ctx| async move {
                let total = items.len() as u64;
                let (mut deleted, mut bytes, mut skipped_protected) = (0u64, 0u64, 0u64);
                let mut failed = Vec::new();
                ctx.progress(0, Some(total), Some("items"), None).await;
                for (i, item) in items.iter().enumerate() {
                    if ctx.is_cancelled() {
                        break;
                    }
                    if item.protected {
                        skipped_protected += 1;
                    } else {
                        match media.delete(&item.server_name, &item.media_id).await {
                            Ok(gone) => {
                                deleted += 1;
                                bytes += gone.size_bytes;
                            }
                            // Deleted meanwhile by somebody else: gone either way.
                            Err(SourceError::NotFound) => {}
                            Err(e) => failed.push(json!({
                                "media_id": item.media_id,
                                "error": e.to_string(),
                            })),
                        }
                    }
                    ctx.progress(i as u64 + 1, Some(total), Some("items"), None)
                        .await;
                }
                tracing::info!(
                    target: "hs_admin::users",
                    user = %owner,
                    deleted,
                    bytes,
                    skipped_protected,
                    failed = failed.len(),
                    "a user's media was deleted"
                );
                events.publish(
                    crate::model::Event::new(
                        "media.deleted",
                        json!({ "uploader": owner, "count": deleted, "bytes": bytes }),
                    )
                    .with_resource(ResourceRef::new("user", owner.clone()))
                    .with_actor(actor),
                );
                Ok(json!({
                    "deleted": deleted,
                    "bytes": bytes,
                    "skipped_protected": skipped_protected,
                    "failed": failed,
                }))
            },
        )
        .await;
    match spawned {
        Ok(task) => accepted(&state, &headers, "users.media.delete", &[], &task),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

/// The body of `users.redact_events`.
#[derive(Debug, Default, Deserialize)]
struct RedactEventsBody {
    room_id: Option<String>,
    reason: Option<String>,
    limit: Option<usize>,
}

/// The most failures a redaction task's result lists one by one.
const MAX_LISTED_FAILURES: usize = 50;

/// `POST /api/v1/users/{user_id}/redact-events` (`moderation:write`, Task): redacts the user's
/// messages (every non-state event of theirs not already redacted), newest first, in one room
/// if `room_id` is given, at most `limit` if given. Each redaction is sent by the user
/// themself while they are still in the room (anyone may redact their own events), and
/// otherwise by the member of this server with the most power in the room, if that is enough
/// to redact; an event nobody here may redact is listed as failed rather than skipped
/// silently. Progress is counted in events.
pub(crate) async fn users_redact_events(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let instance = user_instance(&user_id, "redact-events");
    let principal = match authorize(&state, &headers, Scope::ModerationWrite, &instance).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let request: RedactEventsBody = match parse_optional_json(&body) {
        Ok(r) => r,
        Err(p) => return p.with_instance(instance).into_response(),
    };
    if request.limit == Some(0) {
        return invalid("/limit", "limit must be at least 1")
            .with_instance(instance)
            .into_response();
    }
    if let Err(response) = check_replay(&state, &headers, "users.redact_events", &body, &instance) {
        return response;
    }
    let activity = match activity_source(&state, &instance) {
        Ok(a) => a,
        Err(response) => return response,
    };
    let Some(tasks) = state.tasks.clone() else {
        return unwired("task registry", &instance);
    };
    if let Err(response) = fetch_user(&state, &user_id, &instance).await {
        return response;
    }
    // Chosen now, so that a room that does not exist is a 404 here rather than a failed task,
    // and so that what the task redacts is what was there when the administrator asked.
    let targets = match activity
        .events_to_redact(&user_id, request.room_id.as_deref(), request.limit)
        .await
    {
        Ok(t) => t,
        Err(SourceError::NotFound) => {
            return Problem::not_found()
                .with_detail(format!(
                    "no such room: {}",
                    request.room_id.as_deref().unwrap_or_default()
                ))
                .with_instance(instance)
                .into_response();
        }
        Err(e) => return source_problem(&e, &user_id, &instance),
    };
    if let Err(response) = record(
        &state,
        &principal,
        "users.redact_events",
        "user.redaction_started",
        ResourceRef::new("user", user_id.clone()),
        Vec::new(),
        json!({
            "count": targets.len(),
            "room_id": request.room_id,
            "reason": request.reason,
        }),
        202,
    )
    .await
    {
        return response;
    }
    let events = state.events.clone();
    let actor = principal.to_actor();
    let owner = user_id.clone();
    let reason = request.reason.clone();
    let spawned = tasks
        .spawn(
            "user.redact_events",
            Some(ResourceRef::new("user", user_id.clone())),
            principal.to_actor(),
            move |ctx| async move {
                let total = targets.len() as u64;
                let mut redacted = 0u64;
                let mut failed_count = 0u64;
                let mut failed = Vec::new();
                ctx.progress(0, Some(total), Some("events"), None).await;
                for (i, target) in targets.iter().enumerate() {
                    if ctx.is_cancelled() {
                        break;
                    }
                    match activity.redact_event(target, reason.as_deref()).await {
                        Ok(()) => redacted += 1,
                        Err(e) => {
                            failed_count += 1;
                            tracing::warn!(
                                target: "hs_admin::users",
                                user = %owner,
                                room = %target.room_id,
                                event = %target.event_id,
                                error = %e,
                                "could not redact an event"
                            );
                            if failed.len() < MAX_LISTED_FAILURES {
                                failed.push(json!({
                                    "room_id": target.room_id,
                                    "event_id": target.event_id,
                                    "error": e.to_string(),
                                }));
                            }
                        }
                    }
                    ctx.progress(i as u64 + 1, Some(total), Some("events"), None)
                        .await;
                }
                tracing::info!(
                    target: "hs_admin::users",
                    user = %owner,
                    redacted,
                    failed = failed_count,
                    "a user's events were redacted"
                );
                events.publish(
                    crate::model::Event::new(
                        "user.events_redacted",
                        json!({ "redacted": redacted, "failed": failed_count }),
                    )
                    .with_resource(ResourceRef::new("user", owner.clone()))
                    .with_actor(actor),
                );
                Ok(json!({
                    "total": total,
                    "redacted": redacted,
                    "failed_count": failed_count,
                    "failed": failed,
                }))
            },
        )
        .await;
    match spawned {
        Ok(task) => accepted(&state, &headers, "users.redact_events", &body, &task),
        Err(e) => e.to_problem().with_instance(instance).into_response(),
    }
}

#[cfg(test)]
mod tests;
