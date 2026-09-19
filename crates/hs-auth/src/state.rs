//! [`AuthState`]: the axum shared state every handler and the [`crate::middleware`] extractor in
//! this crate runs against.

use std::sync::{Arc, OnceLock};

use ruma::UserId;

use crate::appservice::{AppserviceRegistry, InMemoryAppserviceRegistry};
use crate::clock::{Clock, SystemClock};
use crate::config::AuthConfig;
use crate::ratelimit::{InMemoryRateLimiter, RateLimiter};
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
    /// Day-one configuration.
    pub config: Arc<AuthConfig>,
    /// The time source, overridden in tests.
    pub clock: Arc<dyn Clock>,
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
            config: Arc::new(AuthConfig::default()),
            clock: Arc::new(SystemClock),
            device_list_notifier: Arc::new(OnceLock::new()),
        }
    }

    /// [`AuthState::in_memory`] with the given config instead of the default.
    #[must_use]
    pub fn in_memory_with_config(config: AuthConfig) -> Self {
        Self {
            config: Arc::new(config),
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
            config: Arc::new(config),
            ..Self::in_memory()
        }
    }

    /// The current time, milliseconds since the Unix epoch, from this state's clock.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// This homeserver's configured server name.
    #[must_use]
    pub fn server_name(&self) -> &ruma::ServerName {
        &self.config.server_name
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
        assert_eq!(state.config.server_name, cloned.config.server_name);
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
}
