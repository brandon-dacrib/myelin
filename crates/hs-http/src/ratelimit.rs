//! A rate-limiter trait shared by the Matrix routes (per-IP and per-user limits) and `/api/v1`
//! (per-principal token bucket, RFC 0004 section 11). The concrete backend (in-memory today,
//! possibly cluster-shared later) is not this crate's concern; this is the seam other tracks
//! implement against.

use std::time::Duration;

/// The outcome of a rate-limit check for one `key` (whatever the caller uses to identify the
/// bucket: a user id, an IP, a token id, `"{principal}:{action_class}"`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The request may proceed. `remaining` is the bucket's remaining capacity, for the
    /// `RateLimit` response header (IETF `draft-ietf-httpapi-ratelimit-headers`).
    Allowed {
        remaining: u32,
        limit: u32,
        reset: Duration,
    },
    /// The request must be rejected with `429`. `retry_after` is how long the caller should wait.
    Limited { retry_after: Duration, limit: u32 },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allowed { .. })
    }

    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Decision::Limited { retry_after, .. } => Some(retry_after.as_millis() as u64),
            Decision::Allowed { .. } => None,
        }
    }
}

/// A rate limiter keyed by an arbitrary string. `check` both tests and consumes one unit of
/// capacity (a "check and take" token bucket), matching how middleware uses it: call once per
/// request, act on the [`Decision`].
#[async_trait::async_trait]
pub trait RateLimiter: Send + Sync {
    async fn check(&self, key: &str) -> Decision;
}

/// A limiter that never limits anything, for tests and for routes that opt out.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unlimited;

#[async_trait::async_trait]
impl RateLimiter for Unlimited {
    async fn check(&self, _key: &str) -> Decision {
        Decision::Allowed {
            remaining: u32::MAX,
            limit: u32::MAX,
            reset: Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_always_allows() {
        let limiter = Unlimited;
        for _ in 0..100 {
            assert!(limiter.check("someone").await.is_allowed());
        }
    }

    #[test]
    fn limited_reports_retry_after_ms() {
        let d = Decision::Limited {
            retry_after: Duration::from_millis(2500),
            limit: 10,
        };
        assert_eq!(d.retry_after_ms(), Some(2500));
        assert!(!d.is_allowed());
    }
}
