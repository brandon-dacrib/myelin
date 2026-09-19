//! Storage traits for users, devices, tokens and user-interactive-auth sessions.
//!
//! Two implementations exist: [`memory::InMemoryAuthStore`] (non-persistent, used by every test in
//! this crate and by anything that wants a store without opening a backend) and
//! [`tables::TablesAuthStore`] (persistent, generic over `hs_kv::KvBackend` — in practice either
//! `hs_kv::memory::MemoryBackend` for fast tests of the tables-backed code path itself, or
//! `hs_kv::fjall_backend::FjallBackend` for `hs serve`). Callers depend on the traits, not a
//! concrete type — they hold `Arc<dyn AuthStore>` (or an individual sub-trait) — so which store
//! backs a running server is a construction-time choice, not something that leaks into call
//! sites. `store::shared_tests` (test-only) runs the same behavioral test suite against both
//! implementations so they cannot silently drift apart.

pub mod memory;
pub mod tables;

#[cfg(test)]
pub(crate) mod shared_tests;

use async_trait::async_trait;
use ruma::{OwnedDeviceId, OwnedUserId};
use thiserror::Error;

use crate::token::TokenHash;

/// Errors from a storage backend. Deliberately thin: callers map these to `M_UNKNOWN`/500 and log
/// the detail, they do not branch on storage internals (that would leak backend choice through
/// the trait boundary).
#[derive(Debug, Error)]
pub enum StoreError {
    /// The operation conflicts with a uniqueness constraint (for example, registering a user ID
    /// that already exists).
    #[error("conflict: {0}")]
    Conflict(String),
    /// The referenced record does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Any other backend failure.
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// A local user account.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserRecord {
    /// The full Matrix user ID (`@localpart:server_name`).
    pub user_id: OwnedUserId,
    /// The password hash (Argon2id PHC string, or an imported bcrypt hash), if the account has a
    /// password. `None` for accounts that only ever log in via SSO/appservice/token.
    pub password_hash: Option<String>,
    /// The server-administrator flag (legacy; distinct from the native OAuth issuer's admin
    /// scopes, see `docs/rfcs/0004-admin-api.md`).
    pub is_admin: bool,
    /// True for a guest account created by `/register?kind=guest`.
    pub is_guest: bool,
    /// Set by `/account/deactivate`. A deactivated account can never log in again.
    pub deactivated: bool,
    /// Set by an administrator. A locked account's existing sessions are rejected with
    /// `M_USER_LOCKED` until unlocked.
    pub locked: bool,
    /// Set by an administrator. A suspended account can read but not perform most writes
    /// (`M_USER_SUSPENDED`); enforcement of which operations are blocked is a per-handler
    /// decision outside this crate's login/token surface.
    pub suspended: bool,
    /// Set by an administrator. Writes from a shadow-banned user appear to succeed to them but
    /// are dropped before reaching other users.
    pub shadow_banned: bool,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// The user's global profile display name (`GET`/`PUT /profile/{userId}/displayname`),
    /// copied into new `m.room.member` events by track 04's room actor. **Not** the same thing as
    /// [`DeviceRecord::display_name`] (a per-device, client-set label shown in a device-manager
    /// UI) or [`DeviceStore::set_display_name`] (that field's setter) -- this is the user's
    /// account-wide profile name shown to other users in rooms. Set with
    /// [`UserStore::set_profile_display_name`], never with the device setter, and vice versa;
    /// the two are unrelated fields on unrelated records that happen to share an English name.
    pub display_name: Option<String>,
    /// The user's global profile avatar `mxc://` URI (`GET`/`PUT /profile/{userId}/avatar_url`),
    /// copied into new `m.room.member` events the same way as [`UserRecord::display_name`]. Set
    /// with [`UserStore::set_profile_avatar_url`].
    pub avatar_url: Option<String>,
}

impl UserRecord {
    /// A fresh, unprivileged, non-guest user record created "now".
    #[must_use]
    pub fn new(user_id: OwnedUserId, created_at_ms: u64) -> Self {
        Self {
            user_id,
            password_hash: None,
            is_admin: false,
            is_guest: false,
            deactivated: false,
            locked: false,
            suspended: false,
            shadow_banned: false,
            created_at_ms,
            display_name: None,
            avatar_url: None,
        }
    }
}

/// A device belonging to a user.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeviceRecord {
    /// The owning user.
    pub user_id: OwnedUserId,
    /// The device id, opaque and client-chosen or server-generated at login/register time.
    pub device_id: OwnedDeviceId,
    /// The client-supplied human-readable name (`initial_device_display_name` at login, or a
    /// later `PUT /devices/{id}`).
    pub display_name: Option<String>,
    /// Last time this device was seen making an authenticated request, milliseconds since epoch.
    pub last_seen_ms: Option<u64>,
    /// Last IP address seen for this device (not exposed to clients; admin/audit use only).
    pub last_seen_ip: Option<String>,
}

/// A stored access token record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AccessTokenRecord {
    /// Hash of the token string (never the token itself).
    pub hash: TokenHash,
    /// The user this token authenticates as.
    pub user_id: OwnedUserId,
    /// The device this token is bound to. `None` for tokens minted without a device (rare; the
    /// legacy admin `register_new_matrix_user` shared-secret flow can produce these).
    pub device_id: Option<OwnedDeviceId>,
    /// Expiry, milliseconds since epoch. `None` means the token does not expire on its own
    /// (typical for a non-refreshable session); refreshable sessions always set this.
    pub expires_at_ms: Option<u64>,
    /// The refresh token that will mint the next access token in this session's chain, if the
    /// session is refreshable.
    pub refresh_token_hash: Option<TokenHash>,
    /// Last time this token was used to authenticate a request, milliseconds since epoch. Used to
    /// expire the *previous* access token in a refresh chain after a grace period, matching
    /// Synapse's `mark_access_token_as_used` bookkeeping.
    pub last_used_ms: Option<u64>,
}

/// A stored refresh token record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RefreshTokenRecord {
    /// Hash of the token string.
    pub hash: TokenHash,
    /// The user this token belongs to.
    pub user_id: OwnedUserId,
    /// The device this token is bound to.
    pub device_id: OwnedDeviceId,
    /// The access token minted alongside this refresh token (for cross-invalidation on logout).
    pub access_token_hash: TokenHash,
    /// Set once this refresh token has been exchanged at `/refresh`. A second exchange of an
    /// already-used refresh token is a reuse signal: the whole chain is revoked, matching the
    /// spec's refresh-token-theft mitigation.
    pub used: bool,
    /// The refresh token that replaced this one, once used.
    pub replaced_by: Option<TokenHash>,
    /// Absolute expiry of this specific refresh token, milliseconds since epoch.
    pub expires_at_ms: Option<u64>,
    /// Absolute expiry of the whole session that this refresh chain may extend to, milliseconds
    /// since epoch (`session_lifetime` in Synapse's config). `None` means unbounded.
    pub ultimate_session_expiry_ms: Option<u64>,
}

/// A stored short-term login token (`m.login.token`) record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LoginTokenRecord {
    /// Hash of the token string.
    pub hash: TokenHash,
    /// The user this token logs in as.
    pub user_id: OwnedUserId,
    /// Absolute expiry, milliseconds since epoch. Synapse defaults this to two minutes after
    /// issuance; login tokens are meant to be redeemed immediately after an SSO or QR-login
    /// redirect.
    pub expires_at_ms: u64,
    /// Set once redeemed. A login token is single-use; a second redemption attempt fails even if
    /// not yet expired.
    pub used: bool,
}

/// User account storage.
#[async_trait]
pub trait UserStore: Send + Sync {
    /// Looks up a user by full Matrix user ID.
    async fn get_user(&self, user_id: &ruma::UserId) -> Result<Option<UserRecord>, StoreError>;

    /// Creates a new user record. Errors with [`StoreError::Conflict`] if the user ID is taken.
    async fn create_user(&self, record: UserRecord) -> Result<(), StoreError>;

    /// True if `localpart` is not yet registered on this server (case-insensitively, matching
    /// Synapse's `/register/available` and login-by-username behavior).
    async fn is_localpart_available(&self, localpart: &str) -> Result<bool, StoreError>;

    /// Sets or clears a user's password hash (used by registration and `/account/password`).
    async fn set_password_hash(
        &self,
        user_id: &ruma::UserId,
        hash: Option<String>,
    ) -> Result<(), StoreError>;

    /// Sets the server-administrator flag.
    async fn set_admin(&self, user_id: &ruma::UserId, admin: bool) -> Result<(), StoreError>;

    /// Sets the locked flag.
    async fn set_locked(&self, user_id: &ruma::UserId, locked: bool) -> Result<(), StoreError>;

    /// Sets the suspended flag.
    async fn set_suspended(
        &self,
        user_id: &ruma::UserId,
        suspended: bool,
    ) -> Result<(), StoreError>;

    /// Sets the deactivated flag. Deactivation is permanent in this trait's contract (no
    /// `reactivate` method); an administrator-driven reactivation is an admin-API concern (15),
    /// not the legacy client surface this crate implements.
    async fn set_deactivated(
        &self,
        user_id: &ruma::UserId,
        deactivated: bool,
    ) -> Result<(), StoreError>;

    /// Sets or clears the user's global profile display name
    /// ([`UserRecord::display_name`]). Errors with [`StoreError::NotFound`] if the user does not
    /// exist.
    ///
    /// Distinct from [`DeviceStore::set_display_name`], which renames one *device*, not the
    /// user's profile; the two names were chosen to read unambiguously side by side at any call
    /// site (`user_store.set_profile_display_name(...)` vs `device_store.set_display_name(...)`).
    async fn set_profile_display_name(
        &self,
        user_id: &ruma::UserId,
        display_name: Option<String>,
    ) -> Result<(), StoreError>;

    /// Sets or clears the user's global profile avatar ([`UserRecord::avatar_url`]). Errors with
    /// [`StoreError::NotFound`] if the user does not exist.
    async fn set_profile_avatar_url(
        &self,
        user_id: &ruma::UserId,
        avatar_url: Option<String>,
    ) -> Result<(), StoreError>;

    /// Binds a third-party identifier (`medium` is `"email"` or `"msisdn"`) to a user, for
    /// `m.login.password` login by email/phone identifier. A day-one, in-crate substitute for the
    /// full 3PID/identity-server flow (`docs/workstreams/07-auth-and-identity.md`'s Phase 1/2
    /// deliverables): no validation session, no identity server, just the bound-address index
    /// login needs. `docs/rfcs/0002-auth-tokens-and-requester.md` section 8 has the follow-up.
    async fn bind_threepid(
        &self,
        user_id: &ruma::UserId,
        medium: &str,
        address: &str,
    ) -> Result<(), StoreError>;

    /// Looks up the user bound to a third-party identifier, if any.
    async fn get_user_by_threepid(
        &self,
        medium: &str,
        address: &str,
    ) -> Result<Option<OwnedUserId>, StoreError>;

    /// Lists every registered user, sorted by `user_id`. Used by the admin API's user directory
    /// ([`crate::admin_directory::AuthStoreUserDirectory`], `hs_admin::sources::UserDirectory`)
    /// and any other bulk-enumeration need.
    ///
    /// # Cost
    /// [`memory::InMemoryAuthStore`]'s implementation clones every stored [`UserRecord`] and
    /// sorts them, O(n log n) in the number of registered users. `tables::TablesAuthStore`'s
    /// implementation is a **full keyspace scan** with no secondary index to narrow it (there is
    /// no bounded "list users" access pattern to index against) -- see that implementation's own
    /// doc comment for the same caveat. Neither implementation paginates; a caller that needs
    /// paginated results (the admin API does) is responsible for slicing the returned `Vec`
    /// itself. Fine for a single operator's user directory; revisit if this server is ever
    /// expected to host enough accounts that a full scan on every `GET /api/v1/users` call
    /// becomes a real cost.
    async fn list_users(&self) -> Result<Vec<UserRecord>, StoreError>;
}

/// Device storage.
#[async_trait]
pub trait DeviceStore: Send + Sync {
    /// Creates a device, or overwrites the display name of one that already exists with this id
    /// (matching Synapse's idempotent `POST /login` device creation).
    async fn upsert_device(&self, record: DeviceRecord) -> Result<(), StoreError>;

    /// Looks up one device.
    async fn get_device(
        &self,
        user_id: &ruma::UserId,
        device_id: &ruma::DeviceId,
    ) -> Result<Option<DeviceRecord>, StoreError>;

    /// Lists every device for a user, ordered by `device_id`.
    async fn list_devices(&self, user_id: &ruma::UserId) -> Result<Vec<DeviceRecord>, StoreError>;

    /// Updates a device's display name. Errors with [`StoreError::NotFound`] if it does not
    /// exist.
    async fn set_display_name(
        &self,
        user_id: &ruma::UserId,
        device_id: &ruma::DeviceId,
        display_name: Option<String>,
    ) -> Result<(), StoreError>;

    /// Deletes a device and, by implication, every token bound to it (the caller is responsible
    /// for also calling [`TokenStore`]'s revocation methods; the two are kept separate so a
    /// device-list read never needs to touch token storage).
    async fn delete_device(
        &self,
        user_id: &ruma::UserId,
        device_id: &ruma::DeviceId,
    ) -> Result<(), StoreError>;

    /// Records that a device was just seen (updates `last_seen_ms`/`last_seen_ip`).
    async fn record_seen(
        &self,
        user_id: &ruma::UserId,
        device_id: &ruma::DeviceId,
        seen_at_ms: u64,
        ip: Option<String>,
    ) -> Result<(), StoreError>;
}

/// Access, refresh and login token storage.
#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Stores a freshly minted access token.
    async fn put_access_token(&self, record: AccessTokenRecord) -> Result<(), StoreError>;

    /// Looks up an access token by its hash.
    async fn get_access_token(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<AccessTokenRecord>, StoreError>;

    /// Deletes an access token (used by `/logout`).
    async fn delete_access_token(&self, hash: &TokenHash) -> Result<(), StoreError>;

    /// Deletes every access token for a user (used by `/logout/all` and account deactivation).
    /// Returns the number of tokens removed.
    async fn delete_all_access_tokens_for_user(
        &self,
        user_id: &ruma::UserId,
    ) -> Result<usize, StoreError>;

    /// Deletes every access token for a user except one (used by `/account/password` with
    /// `logout_devices: true`, which keeps the session that made the change alive).
    async fn delete_other_access_tokens_for_user(
        &self,
        user_id: &ruma::UserId,
        except: &TokenHash,
    ) -> Result<usize, StoreError>;

    /// Deletes every access token for one device of a user (used by `/delete_devices` and
    /// per-device `DELETE /devices/{id}`).
    async fn delete_access_tokens_for_device(
        &self,
        user_id: &ruma::UserId,
        device_id: &ruma::DeviceId,
    ) -> Result<(), StoreError>;

    /// Marks an access token as used just now (bookkeeping for refresh-chain grace periods).
    async fn mark_access_token_used(
        &self,
        hash: &TokenHash,
        used_at_ms: u64,
    ) -> Result<(), StoreError>;

    /// Stores a freshly minted refresh token.
    async fn put_refresh_token(&self, record: RefreshTokenRecord) -> Result<(), StoreError>;

    /// Looks up a refresh token by its hash.
    async fn get_refresh_token(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<RefreshTokenRecord>, StoreError>;

    /// Marks a refresh token as used and records what replaced it (rotation).
    async fn mark_refresh_token_used(
        &self,
        hash: &TokenHash,
        replaced_by: TokenHash,
    ) -> Result<(), StoreError>;

    /// Deletes a refresh token outright (used when revoking a session, as opposed to rotating
    /// it).
    async fn delete_refresh_token(&self, hash: &TokenHash) -> Result<(), StoreError>;

    /// Deletes every refresh token for a user (used by `/logout/all` and account deactivation, to
    /// close the window a lingering refresh token would otherwise leave open).
    async fn delete_all_refresh_tokens_for_user(
        &self,
        user_id: &ruma::UserId,
    ) -> Result<usize, StoreError>;

    /// Stores a freshly minted short-term login token.
    async fn put_login_token(&self, record: LoginTokenRecord) -> Result<(), StoreError>;

    /// Atomically looks up and consumes a login token: returns it only if it existed, was not
    /// expired and had not already been used, and marks it used in the same operation so a
    /// concurrent second redemption cannot also succeed.
    async fn consume_login_token(
        &self,
        hash: &TokenHash,
        now_ms: u64,
    ) -> Result<Option<LoginTokenRecord>, StoreError>;
}

/// User-interactive-auth session storage. See [`crate::uia`] for the state machine built on top
/// of this trait.
#[async_trait]
pub trait UiaStore: Send + Sync {
    /// Creates a new UIA session and returns its freshly generated session id.
    async fn create_session(&self, created_at_ms: u64) -> Result<String, StoreError>;

    /// Records that `stage` completed successfully for `session_id`, with whatever
    /// stage-specific result data is worth remembering (for password stages, nothing beyond
    /// "done"; for 3PID stages, the validated address).
    async fn mark_stage_complete(&self, session_id: &str, stage: &str) -> Result<(), StoreError>;

    /// The set of stages completed so far in this session.
    async fn completed_stages(&self, session_id: &str) -> Result<Vec<String>, StoreError>;

    /// Arbitrary key-value data a handler wants to remember across UIA round-trips for one
    /// session (for example, the registration parameters submitted on the first call, so the
    /// final call — after the last stage completes — can still see them).
    async fn set_session_data(
        &self,
        session_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), StoreError>;

    /// Reads session data set by [`UiaStore::set_session_data`].
    async fn get_session_data(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, StoreError>;

    /// True if `session_id` was created by [`UiaStore::create_session`] and has not expired.
    async fn session_exists(
        &self,
        session_id: &str,
        now_ms: u64,
        timeout_ms: u64,
    ) -> Result<bool, StoreError>;
}

/// The union of every storage trait this crate needs, for callers that just want "the auth
/// store" without naming each capability. [`memory::InMemoryAuthStore`] implements all four.
pub trait AuthStore: UserStore + DeviceStore + TokenStore + UiaStore {}
impl<T: UserStore + DeviceStore + TokenStore + UiaStore> AuthStore for T {}
