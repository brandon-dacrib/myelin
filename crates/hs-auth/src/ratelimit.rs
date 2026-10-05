//! Rate limiting behind a trait, so the legacy endpoints (`/login`, `/register`, `/account/password`,
//! ...) never call a concrete limiter directly. Matches Synapse's `rc_login`/`rc_registration`
//! shape (per-key token bucket with a burst) closely enough that the config track (13) can map
//! Synapse's `homeserver.yaml` rate-limit sections onto whatever implements this trait.

use std::collections::HashMap;
use std::sync::Mutex;

/// A rate limiter keyed by an arbitrary string (an IP address, a user ID, an appservice ID —
/// callers decide what "one limited entity" means for a given endpoint).
pub trait RateLimiter: Send + Sync {
    /// Attempts to consume one unit of the bucket named `key` at `now_ms`. Returns `Ok(())` if
    /// allowed, or `Err(retry_after_ms)` — how long the caller should wait before trying again —
    /// if the bucket is empty.
    fn check(&self, key: &str, now_ms: u64) -> Result<(), u64>;
}

/// A classic token bucket: `burst` capacity, refilling at `per_second` tokens per second.
#[derive(Debug, Clone, Copy)]
pub struct TokenBucketConfig {
    /// Tokens refilled per second.
    pub per_second: f64,
    /// Maximum tokens a bucket can hold (the burst allowance).
    pub burst: f64,
}

impl TokenBucketConfig {
    /// A limiter that never blocks (`burst` effectively infinite), for tests and for endpoints
    /// operators have configured with no limit.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            per_second: f64::MAX,
            burst: f64::MAX,
        }
    }
}

struct Bucket {
    tokens: f64,
    last_refill_ms: u64,
}

/// An in-memory, single-process token-bucket [`RateLimiter`]. Fine for the in-memory backend and
/// for a single-node deployment; a clustered deployment will want a store-backed implementation
/// (track 03's ownership model makes per-owner in-memory limiting for room/user-scoped operations
/// viable too, but that is a placement decision for whoever wires this trait up, not for this
/// crate).
pub struct InMemoryRateLimiter {
    config: TokenBucketConfig,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl InMemoryRateLimiter {
    /// A limiter with the given bucket shape.
    #[must_use]
    pub fn new(config: TokenBucketConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// A limiter that never blocks.
    #[must_use]
    pub fn unlimited() -> Self {
        Self::new(TokenBucketConfig::unlimited())
    }
}

impl RateLimiter for InMemoryRateLimiter {
    fn check(&self, key: &str, now_ms: u64) -> Result<(), u64> {
        if self.config.burst.is_infinite() {
            return Ok(());
        }
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bucket = buckets.entry(key.to_string()).or_insert_with(|| Bucket {
            tokens: self.config.burst,
            last_refill_ms: now_ms,
        });
        let elapsed_ms = now_ms.saturating_sub(bucket.last_refill_ms);
        let refilled = (elapsed_ms as f64 / 1000.0) * self.config.per_second;
        bucket.tokens = (bucket.tokens + refilled).min(self.config.burst);
        bucket.last_refill_ms = now_ms;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let deficit = 1.0 - bucket.tokens;
            let wait_ms = (deficit / self.config.per_second * 1000.0).ceil() as u64;
            Err(wait_ms)
        }
    }
}

/// The server-wide `rate_limits.*` buckets enforced by this crate's routes and by the crates
/// whose state embeds [`crate::state::AuthState`] (`hs-room`): one
/// [`hs_http::buckets::TokenBuckets`] per configured bucket, each of which a running server
/// re-points when its setting changes. All limit nothing until set.
///
/// `message` is not here: `hs_room::moderation::SendLimiter` enforces it alongside
/// administrators' per-user overrides. `federation` is not here either: the federation
/// transport, which does not see this state, holds its own.
#[derive(Debug)]
pub struct ServerLimits {
    /// `rate_limits.login`: `POST /login`, per client address.
    pub login: hs_http::buckets::TokenBuckets,
    /// `rate_limits.registration`: accounts made through `POST /register`, per client address.
    pub registration: hs_http::buckets::TokenBuckets,
    /// `rate_limits.joins_local`: joins to rooms this server already hosts, per user.
    pub joins_local: hs_http::buckets::TokenBuckets,
    /// `rate_limits.joins_remote`: joins made through another server, per user.
    pub joins_remote: hs_http::buckets::TokenBuckets,
    /// `rate_limits.admin_redaction`: redactions by a server administrator, per user, in place of
    /// the message limit (as Synapse's `rc_admin_redaction`).
    pub admin_redaction: hs_http::buckets::TokenBuckets,
    /// `rate_limits.third_party_id_validation`: validation emails requested
    /// (`POST /register/email/requestToken` and its siblings), per client address and per
    /// address emailed, as Synapse's `rc_3pid_validation`.
    pub third_party_id_validation: hs_http::buckets::TokenBuckets,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            login: hs_http::buckets::TokenBuckets::new("login"),
            registration: hs_http::buckets::TokenBuckets::new("registration"),
            joins_local: hs_http::buckets::TokenBuckets::new("joins_local"),
            joins_remote: hs_http::buckets::TokenBuckets::new("joins_remote"),
            admin_redaction: hs_http::buckets::TokenBuckets::new("admin_redaction"),
            third_party_id_validation: hs_http::buckets::TokenBuckets::new(
                "third_party_id_validation",
            ),
        }
    }
}

impl ServerLimits {
    /// Sets every bucket from the configuration's `rate_limits`: each bucket's own limit while
    /// `rate_limits.enabled`, and no limit at all otherwise.
    pub fn apply(&self, config: &hs_config::RateLimitConfig) {
        let limit = |bucket: &hs_config::ratelimit::RateLimitBucket| {
            config.enabled.then_some(hs_http::buckets::BucketLimit {
                per_second: bucket.per_second,
                burst_count: bucket.burst_count,
            })
        };
        self.login.set_limit(limit(&config.login));
        self.registration.set_limit(limit(&config.registration));
        self.joins_local.set_limit(limit(&config.joins_local));
        self.joins_remote.set_limit(limit(&config.joins_remote));
        self.admin_redaction
            .set_limit(limit(&config.admin_redaction));
        self.third_party_id_validation
            .set_limit(limit(&config.third_party_id_validation));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_limits_follow_the_configuration_and_its_master_switch() {
        let limits = ServerLimits::default();
        assert!(limits.login.limit().is_none(), "unlimited until set");
        let mut config = hs_config::RateLimitConfig::default();
        config.login.burst_count = 1;
        limits.apply(&config);
        assert_eq!(limits.login.limit().unwrap().burst_count, 1);
        assert!(limits.login.take("a", 0).is_ok());
        assert!(limits.login.take("a", 0).is_err());
        config.enabled = false;
        limits.apply(&config);
        assert!(limits.login.take("a", 0).is_ok());
    }

    #[test]
    fn burst_allows_that_many_requests_immediately() {
        let limiter = InMemoryRateLimiter::new(TokenBucketConfig {
            per_second: 1.0,
            burst: 3.0,
        });
        assert!(limiter.check("k", 0).is_ok());
        assert!(limiter.check("k", 0).is_ok());
        assert!(limiter.check("k", 0).is_ok());
        assert!(limiter.check("k", 0).is_err());
    }

    #[test]
    fn tokens_refill_over_time() {
        let limiter = InMemoryRateLimiter::new(TokenBucketConfig {
            per_second: 1.0,
            burst: 1.0,
        });
        assert!(limiter.check("k", 0).is_ok());
        assert!(limiter.check("k", 100).is_err());
        assert!(limiter.check("k", 1000).is_ok());
    }

    #[test]
    fn different_keys_are_independent() {
        let limiter = InMemoryRateLimiter::new(TokenBucketConfig {
            per_second: 1.0,
            burst: 1.0,
        });
        assert!(limiter.check("a", 0).is_ok());
        assert!(limiter.check("b", 0).is_ok());
    }

    #[test]
    fn unlimited_never_blocks() {
        let limiter = InMemoryRateLimiter::unlimited();
        for _ in 0..1000 {
            assert!(limiter.check("k", 0).is_ok());
        }
    }

    #[test]
    fn retry_after_is_positive_when_blocked() {
        let limiter = InMemoryRateLimiter::new(TokenBucketConfig {
            per_second: 2.0,
            burst: 1.0,
        });
        assert!(limiter.check("k", 0).is_ok());
        let err = limiter.check("k", 0).unwrap_err();
        assert!(err > 0);
    }
}
