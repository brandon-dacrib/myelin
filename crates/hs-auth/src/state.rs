//! [`AuthState`]: the axum shared state every handler and the [`crate::middleware`] extractor in
//! this crate runs against.

use std::sync::{Arc, OnceLock};

use ruma::UserId;

use crate::appservice::{AppserviceRegistry, InMemoryAppserviceRegistry};
use crate::clock::{Clock, SystemClock};
use crate::config::AuthConfig;
use crate::ratelimit::{InMemoryRateLimiter, RateLimiter, ServerLimits};
use crate::registration_tokens::{InMemoryRegistrationTokens, RegistrationTokenStore};
use crate::store::AuthStore;
use crate::store::memory::InMemoryAuthStore;

/// A hook this crate's device routes (`crate::routes::devices`: rename, delete, bulk delete) call
/// whenever a device identity change should be visible to other users' device-list tracking
/// (`/keys/changes`, `/sync`'s `device_lists`). `hs-auth` cannot depend on `hs-e2e` (`hs-e2e`
/// already depends on `hs-auth` for `AuthState`/`Requester`; the reverse would be a cycle), so
/// this trait is defined here and `hs-e2e` installs a real implementation via
/// [`AuthState::install_device_list_notifier`] when it builds its own state
/// (`hs_e2e::state::E2eState::new`) — mirroring `hs_room::registry::RoomRegistry`'s
/// `GlobalTokenResolver` and `hs_e2e::state::E2eState`'s own `SyncTokenResolver`, the same shape
/// of problem solved twice already elsewhere in this workspace. Unset (no installer) means
/// nothing else in this process cares about device-list changes, in which case the notify calls
/// below are silent no-ops -- exactly like those two other hooks when nothing has installed them.
#[async_trait::async_trait]
pub trait DeviceListChangeNotifier: Send + Sync {
    /// Records that `user_id`'s device list changed (a device was renamed, added or removed).
    async fn notify_device_list_changed(&self, user_id: &UserId);
}

/// Told when a user's other sessions are revoked (`POST /account/password` with
/// `logout_devices`, the default): the push layer removes the pushers those sessions
/// registered, as the spec asks, since a pusher belongs to the login that made it. Defined here
/// and answered from outside, like [`DeviceListChangeNotifier`], because this crate cannot see
/// pushers.
#[async_trait::async_trait]
pub trait SessionRevocationObserver: Send + Sync {
    /// Every session of `user_id` but `kept_device`'s (`None`: every session) was revoked.
    async fn other_sessions_revoked(&self, user_id: &UserId, kept_device: Option<&ruma::DeviceId>);
}

/// Who `POST /user_directory/search` may show to whom.
///
/// The spec's floor for that endpoint is "the users the requesting user shares a room with and
/// those who reside in public rooms", and that floor is also the ceiling a server should want by
/// default: a directory that searches every account lets any account enumerate every other
/// user's name. But "shares a room" and "public room" are facts about rooms, and this crate
/// cannot see rooms (`hs-user` depends on it, not the other way round) -- so, like
/// [`DeviceListChangeNotifier`], the question is a trait defined here and answered from outside
/// (`hs_user::hub::SessionHub`, installed by `hs serve`).
#[async_trait::async_trait]
pub trait UserDirectoryVisibility: Send + Sync {
    /// Every user `requester` may find -- themself included when they are in a public room, as
    /// Synapse answers (Sytest's user-directory tests search for the requester's own name).
    ///
    /// # Errors
    /// A description of why the room layer could not answer. The search then fails rather than
    /// falling back to showing everybody.
    async fn visible_to(
        &self,
        requester: &UserId,
    ) -> Result<std::collections::BTreeSet<ruma::OwnedUserId>, String>;
}

/// Where a profile change an administrator makes (`hs-admin`'s `users.update`, through
/// [`crate::admin_directory::AuthStoreUserDirectory`]) is carried into the user's rooms: the
/// same re-stamping of their `m.room.member` event in every joined room that the client's own
/// `PUT /profile/{userId}/displayname` does (`hs_room::routes::profile`), which is the only way
/// other clients, and other servers, ever learn a name or avatar changed. This crate cannot
/// reach rooms (`hs-room` depends on it, not the reverse), so, like [`RemoteProfileSource`],
/// the fan-out is a trait defined here and installed by `hs serve` through
/// [`AuthState::install_profile_refresh`]. Unset, the record changes and the rooms do not
/// (a test of this crate alone); the directory logs that at debug level.
pub trait ProfileRefresh: Send + Sync {
    /// `user_id`'s stored profile changed; re-stamp their membership in every room they are
    /// joined to. Fire-and-forget: the implementation spawns the work and returns at once, and
    /// reads the profile itself (from `auth`) rather than taking the values, so two quick
    /// changes cannot race to write a stale one. `auth` is passed per call, not held, because
    /// this hook lives inside an [`AuthState`]: holding one would be a reference cycle that keeps
    /// the stores alive after shutdown.
    fn profile_changed(&self, auth: &AuthState, user_id: &UserId);
}

/// Where `GET /profile/{userId}`, `/displayname` and `/avatar_url` get the profile of a user of
/// another server: that server, over federation (`GET /_matrix/federation/v1/query/profile`).
/// This crate cannot speak federation (`hs-federation` is a peer, and `hs serve` is where the
/// two meet), so, like [`UserDirectoryVisibility`], the question is a trait defined here and
/// answered from outside, installed by `hs serve` through [`AuthState::install_remote_profiles`]
/// when federation is on. Unset, a remote user's profile is `404 M_NOT_FOUND`, as before.
#[async_trait::async_trait]
pub trait RemoteProfileSource: Send + Sync {
    /// `user_id`'s profile as their server answers it, narrowed to `field` (`displayname` or
    /// `avatar_url`) when one is given. `Ok(None)` when that server does not know the user.
    ///
    /// # Errors
    /// Why the server could not be asked, or answered with something other than a profile or a
    /// `404`; the client gets `502`.
    async fn remote_profile(
        &self,
        user_id: &UserId,
        field: Option<&str>,
    ) -> Result<Option<serde_json::Value>, String>;
}

/// Everything a handler needs: storage, the appservice registry, rate limiting, config and a
/// clock, all behind `Arc` so `AuthState` itself is cheap to clone (axum requires `State<S>: Clone`).
#[derive(Clone)]
pub struct AuthState {
    /// User, device, token and UIA session storage.
    pub store: Arc<dyn AuthStore>,
    /// Application service token lookup (track 11's stub, see [`crate::appservice`]).
    pub appservices: Arc<dyn AppserviceRegistry>,
    /// Rate limiting, keyed per endpoint by whatever the handler considers "one entity".
    pub rate_limiter: Arc<dyn RateLimiter>,
    /// This crate's configuration, read through a [`hs_config::Live`] cell so a running server
    /// can replace it (`enable_registration`, the password policy, token lifetimes, ...): every
    /// clone of this state shares the cell, and a handler sees a change on its next
    /// `config.get()`. The server name in it never changes (it is bootstrap).
    pub config: hs_config::Live<AuthConfig>,
    /// The server-wide `rate_limits.*` buckets this crate and the crates that embed this state
    /// enforce (`login`, `registration`, joins, administrators' redactions). Limit nothing until
    /// a server sets them (`hs serve` does, from the configuration, and again on every change).
    pub limits: Arc<ServerLimits>,
    /// This server's name, copied out of [`AuthConfig::server_name`] when the state is built so
    /// [`AuthState::server_name`] can lend it.
    pub(crate) server_name: ruma::OwnedServerName,
    /// The time source, overridden in tests.
    pub clock: Arc<dyn Clock>,
    /// The registration tokens `/register`'s `m.login.registration_token` stage accepts, and the
    /// admin API's `registration_tokens.*` operations manage (see
    /// [`crate::registration_tokens`]). In memory unless replaced with
    /// [`AuthState::with_registration_tokens`].
    pub registration_tokens: Arc<dyn RegistrationTokenStore>,
    /// See [`DeviceListChangeNotifier`] and [`AuthState::install_device_list_notifier`].
    /// `Arc`-wrapped around the `OnceLock` (not a bare `OnceLock` field) so every clone of this
    /// state produced by axum's per-request `Clone` -- and every clone taken before installation,
    /// such as the one `hs-room`'s `RoomState` embeds -- shares the same cell: installing the
    /// notifier once, on any clone, makes it visible to all of them, the same reasoning
    /// `hs_e2e::state::E2eState::sync_token_resolver`'s doc comment gives for its identical field.
    // `pub(crate)`, not private: several of this crate's own test modules (`crate::middleware`)
    // build a variant `AuthState` via struct-update syntax (`AuthState { appservices: ..,
    // ..state }`) from a sibling module, which needs every field nameable from within the crate,
    // not just this one's own module. External crates still cannot name this field directly (only
    // `pub` fields can be set from outside `hs-auth`); they go through
    // [`AuthState::install_device_list_notifier`] regardless.
    pub(crate) device_list_notifier: Arc<OnceLock<Arc<dyn DeviceListChangeNotifier>>>,
    /// See [`UserDirectoryVisibility`] and [`AuthState::install_user_directory_visibility`].
    pub(crate) user_directory_visibility: Arc<OnceLock<Arc<dyn UserDirectoryVisibility>>>,
    /// See [`SessionRevocationObserver`] and [`AuthState::install_session_revocation_observer`].
    pub(crate) session_revocation_observer: Arc<OnceLock<Arc<dyn SessionRevocationObserver>>>,
    /// See [`RemoteProfileSource`] and [`AuthState::install_remote_profiles`].
    pub(crate) remote_profiles: Arc<OnceLock<Arc<dyn RemoteProfileSource>>>,
    /// See [`ProfileRefresh`] and [`AuthState::install_profile_refresh`].
    pub(crate) profile_refresh: Arc<OnceLock<Arc<dyn ProfileRefresh>>>,
}

impl AuthState {
    /// The in-memory, single-process stack: [`InMemoryAuthStore`], an empty
    /// [`InMemoryAppserviceRegistry`], an unlimited rate limiter, default config and the real
    /// system clock. What every route handler test in this crate builds unless it needs to
    /// override one piece.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            store: Arc::new(InMemoryAuthStore::new()),
            appservices: Arc::new(InMemoryAppserviceRegistry::new()),
            rate_limiter: Arc::new(InMemoryRateLimiter::unlimited()),
            config: hs_config::Live::new(AuthConfig::default()),
            limits: Arc::new(ServerLimits::default()),
            server_name: AuthConfig::default().server_name,
            clock: Arc::new(SystemClock),
            registration_tokens: Arc::new(InMemoryRegistrationTokens::new()),
            device_list_notifier: Arc::new(OnceLock::new()),
            user_directory_visibility: Arc::new(OnceLock::new()),
            session_revocation_observer: Arc::new(OnceLock::new()),
            remote_profiles: Arc::new(OnceLock::new()),
            profile_refresh: Arc::new(OnceLock::new()),
        }
    }

    /// [`AuthState::in_memory`] with the given config instead of the default.
    #[must_use]
    pub fn in_memory_with_config(config: AuthConfig) -> Self {
        Self {
            server_name: config.server_name.clone(),
            config: hs_config::Live::new(config),
            ..Self::in_memory()
        }
    }

    /// Builds an `AuthState` around an already-open store (in practice
    /// [`crate::store::tables::TablesAuthStore`] over a real `hs_kv::KvBackend`, for a server
    /// that must survive a restart) and the given config, keeping every other piece
    /// ([`InMemoryAppserviceRegistry`], an unlimited rate limiter, the real system clock) the
    /// same as [`AuthState::in_memory`]. This is the constructor a real `hs serve` process
    /// should use once it has opened a persistent backend — see
    /// `docs/status/07-auth-and-identity.md`'s "Interfaces provided" for the exact call this
    /// replaces (`AuthState::in_memory_with_config`) and what `hs-cli`'s `serve.rs` needs to
    /// change to use it.
    #[must_use]
    pub fn with_store(store: Arc<dyn AuthStore>, config: AuthConfig) -> Self {
        Self {
            store,
            server_name: config.server_name.clone(),
            config: hs_config::Live::new(config),
            ..Self::in_memory()
        }
    }

    /// Replaces the appservice registry, returning the state so this reads as a builder.
    ///
    /// This exists because [`AuthState`] has a private field (the device-list notifier hook), and
    /// Rust forbids functional-update syntax — `AuthState { appservices, ..AuthState::in_memory() }`
    /// — on a struct with a private field from outside the defining crate. Two crates were doing
    /// exactly that, and adding the hook broke both of their test harnesses, which only a
    /// workspace-wide build catches: the crates that own the field build perfectly on their own.
    ///
    /// Swapping the registry is the only reason anything outside this crate ever wanted to
    /// construct an `AuthState` field by field, so this is the whole of the API that was missing.
    #[must_use]
    pub fn with_appservices(mut self, appservices: Arc<dyn AppserviceRegistry>) -> Self {
        self.appservices = appservices;
        self
    }

    /// Replaces the registration-token store (a real server's durable one:
    /// [`crate::registration_tokens::TablesRegistrationTokens`]), returning the state so this
    /// reads as a builder. For the same reason as [`AuthState::with_appservices`].
    #[must_use]
    pub fn with_registration_tokens(mut self, tokens: Arc<dyn RegistrationTokenStore>) -> Self {
        self.registration_tokens = tokens;
        self
    }

    /// The current time, milliseconds since the Unix epoch, from this state's clock.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// This homeserver's configured server name.
    #[must_use]
    pub fn server_name(&self) -> &ruma::ServerName {
        &self.server_name
    }

    /// Replaces this state's configuration for every clone of it, keeping the server name (which
    /// is fixed for the life of a database). What a running server calls when an administrator
    /// changes an `auth` setting.
    pub fn set_config(&self, mut config: AuthConfig) {
        config.server_name = self.server_name.clone();
        self.config.set(config);
    }

    /// Installs the [`DeviceListChangeNotifier`] `crate::routes::devices`' rename/delete/bulk
    /// delete handlers call after mutating a device. Idempotent past the first call: a second
    /// install is silently ignored (logged, not panicked), matching
    /// `hs_room::registry::RoomRegistry::install_global_token_resolver`'s same convention -- one
    /// state is expected to have exactly one installer for the lifetime of the process.
    pub fn install_device_list_notifier(&self, notifier: Arc<dyn DeviceListChangeNotifier>) {
        if self.device_list_notifier.set(notifier).is_err() {
            tracing::warn!(
                "a device-list change notifier was already installed on this auth state; \
                 ignoring the second install"
            );
        }
    }

    /// Installs what scopes `POST /user_directory/search` to the users its caller may see.
    /// Same one-installer convention as [`AuthState::install_device_list_notifier`].
    pub fn install_user_directory_visibility(&self, visibility: Arc<dyn UserDirectoryVisibility>) {
        if self.user_directory_visibility.set(visibility).is_err() {
            tracing::warn!(
                "a user-directory visibility source was already installed on this auth state; \
                 ignoring the second install"
            );
        }
    }

    /// Installs what is told when a user's other sessions are revoked. Same one-installer
    /// convention as [`AuthState::install_device_list_notifier`].
    pub fn install_session_revocation_observer(
        &self,
        observer: Arc<dyn SessionRevocationObserver>,
    ) {
        if self.session_revocation_observer.set(observer).is_err() {
            tracing::warn!(
                "a session revocation observer was already installed on this auth state; \
                 ignoring the second install"
            );
        }
    }

    /// Calls the installed [`SessionRevocationObserver`], if any; a no-op otherwise.
    pub async fn notify_other_sessions_revoked(
        &self,
        user_id: &UserId,
        kept_device: Option<&ruma::DeviceId>,
    ) {
        if let Some(observer) = self.session_revocation_observer.get() {
            observer.other_sessions_revoked(user_id, kept_device).await;
        }
    }

    /// The installed [`UserDirectoryVisibility`], if any. `None` means this process has no room
    /// layer at all (a test of this crate alone), and so no rooms for anybody to be private in.
    #[must_use]
    pub fn user_directory_visibility(&self) -> Option<&Arc<dyn UserDirectoryVisibility>> {
        self.user_directory_visibility.get()
    }

    /// Installs where a remote user's profile is asked for (`hs serve`, over federation). Same
    /// one-installer convention as [`AuthState::install_device_list_notifier`].
    pub fn install_remote_profiles(&self, source: Arc<dyn RemoteProfileSource>) {
        if self.remote_profiles.set(source).is_err() {
            tracing::warn!(
                "a remote profile source was already installed on this auth state; ignoring the \
                 second install"
            );
        }
    }

    /// The installed [`RemoteProfileSource`], if any. `None` means this process cannot ask
    /// other servers (federation off, or a test of this crate alone).
    #[must_use]
    pub fn remote_profiles(&self) -> Option<&Arc<dyn RemoteProfileSource>> {
        self.remote_profiles.get()
    }

    /// Installs where an administrator's profile change is carried into the user's rooms
    /// (`hs serve`, from the room layer). Same one-installer convention as
    /// [`AuthState::install_remote_profiles`].
    pub fn install_profile_refresh(&self, refresh: Arc<dyn ProfileRefresh>) {
        if self.profile_refresh.set(refresh).is_err() {
            tracing::warn!(
                "a profile refresh was already installed on this auth state; ignoring the \
                 second install"
            );
        }
    }

    /// The installed [`ProfileRefresh`], if any. `None` means this process has no room layer
    /// (a test of this crate alone), so a profile change stays in the record.
    #[must_use]
    pub fn profile_refresh(&self) -> Option<&Arc<dyn ProfileRefresh>> {
        self.profile_refresh.get()
    }

    /// The installed [`DeviceListChangeNotifier`], if any.
    #[must_use]
    pub fn device_list_notifier(&self) -> Option<&Arc<dyn DeviceListChangeNotifier>> {
        self.device_list_notifier.get()
    }

    /// Calls the installed [`DeviceListChangeNotifier`], if any -- a silent no-op otherwise (no
    /// other crate cares about device-list changes in this process, for example a test that never
    /// constructs `hs-e2e`'s state at all). Handlers call this instead of checking
    /// [`AuthState::device_list_notifier`] themselves.
    pub async fn notify_device_list_changed(&self, user_id: &UserId) {
        if let Some(notifier) = self.device_list_notifier() {
            notifier.notify_device_list_changed(user_id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_state_is_cloneable_and_usable() {
        let state = AuthState::in_memory();
        let cloned = state.clone();
        assert_eq!(
            state.config.get().server_name,
            cloned.config.get().server_name
        );
    }

    #[test]
    fn with_store_uses_the_given_store_and_config() {
        use crate::store::tables::TablesAuthStore;
        let backend = hs_kv::memory::MemoryBackend::new();
        let store: Arc<dyn AuthStore> = Arc::new(TablesAuthStore::open(backend).unwrap());
        let config = AuthConfig {
            server_name: ruma::server_name!("with-store.example.org").to_owned(),
            ..AuthConfig::default()
        };
        let state = AuthState::with_store(store, config);
        assert_eq!(state.server_name(), "with-store.example.org");
    }

    #[test]
    fn a_new_config_reaches_every_clone_and_keeps_the_server_name() {
        let state = AuthState::in_memory();
        let clone = state.clone();
        assert!(clone.config.get().registration_enabled);
        state.set_config(AuthConfig {
            registration_enabled: false,
            server_name: ruma::server_name!("elsewhere.example").to_owned(),
            ..AuthConfig::default()
        });
        assert!(!clone.config.get().registration_enabled);
        assert_eq!(clone.config.get().server_name, "example.org");
    }
}
