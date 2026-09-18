//! [`AuthState`]: the axum shared state every handler and the [`crate::middleware`] extractor in
//! this crate runs against.

use std::sync::Arc;

use crate::appservice::{AppserviceRegistry, InMemoryAppserviceRegistry};
use crate::clock::{Clock, SystemClock};
use crate::config::AuthConfig;
use crate::ratelimit::{InMemoryRateLimiter, RateLimiter};
use crate::store::AuthStore;
use crate::store::memory::InMemoryAuthStore;

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
}
