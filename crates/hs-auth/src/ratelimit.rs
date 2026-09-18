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

#[cfg(test)]
mod tests {
    use super::*;

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
