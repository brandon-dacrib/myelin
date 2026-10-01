//! Server-wide rate-limit buckets whose limit can change while the server runs, and the client
//! address they are keyed by.
//!
//! [`TokenBuckets`] is what the configuration's `rate_limits.*` buckets are enforced with (all
//! but `message`, which `hs_room::moderation::SendLimiter` enforces alongside administrators'
//! per-user overrides). [`ClientIp`] is the extractor the per-address buckets (`login`,
//! `registration`) key by.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError, RwLock};

use axum::extract::{ConnectInfo, FromRequestParts};
use http::request::Parts;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;

/// One token-bucket limit: `burst_count` requests at once, refilled at `per_second`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketLimit {
    /// Tokens refilled per second. `0` (or less) limits nothing.
    pub per_second: f64,
    /// The most requests allowed at once (at least one).
    pub burst_count: u32,
}

struct KeyedBucket {
    tokens: f64,
    last_ms: u64,
}

/// The labels of `hs_rate_limited_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct LimitedLabels {
    bucket: &'static str,
}

/// Process-wide: a counter is an atomic, and the buckets are checked far from any registry.
static LIMITED: LazyLock<Family<LimitedLabels, Counter>> = LazyLock::new(Family::default);

/// Registers `hs_rate_limited_total{bucket}` -- requests a [`TokenBuckets`] refused, by bucket
/// name -- into `registry`.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_rate_limited",
        "Requests refused with 429 by a server-wide rate-limit bucket (rate_limits.*), by bucket",
        LIMITED.clone(),
    );
}

/// How many keys a [`TokenBuckets`] remembers before it forgets the ones that have refilled
/// (which are indistinguishable from keys it never saw).
const PRUNE_ABOVE: usize = 4096;

/// Token buckets keyed by whatever one limited entity is (a client address, a user, an origin
/// server), all under one limit that can be replaced while the server runs
/// ([`TokenBuckets::set_limit`]) -- the configuration's `rate_limits.*` buckets.
///
/// In-process: in cluster mode each replica limits what it handles, as the message limit does
/// (decision 0016). A bucket keeps what it has left when the limit changes, clamped to the new
/// burst, so lowering a limit bites at once rather than handing everybody a fresh burst first.
/// With no limit (the default, and `rate_limits.enabled: false`) nothing is refused.
pub struct TokenBuckets {
    name: &'static str,
    limit: RwLock<Option<BucketLimit>>,
    buckets: Mutex<HashMap<String, KeyedBucket>>,
}

impl std::fmt::Debug for TokenBuckets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenBuckets")
            .field("name", &self.name)
            .field("limit", &self.limit())
            .finish_non_exhaustive()
    }
}

impl TokenBuckets {
    /// Buckets named `name` (the `bucket` label of `hs_rate_limited_total`, and what a log line
    /// calls them), limiting nothing until [`Self::set_limit`].
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            limit: RwLock::new(None),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// The name these buckets were made with.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Replaces the limit. `None`, or a `per_second` of `0`, limits nobody (and forgets every
    /// bucket). Takes effect on the next check.
    pub fn set_limit(&self, limit: Option<BucketLimit>) {
        let limit = limit.filter(|l| l.per_second > 0.0);
        *self.limit.write().unwrap_or_else(PoisonError::into_inner) = limit;
        if limit.is_none() {
            self.lock().clear();
        }
    }

    /// The limit in force.
    #[must_use]
    pub fn limit(&self) -> Option<BucketLimit> {
        *self.limit.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, KeyedBucket>> {
        self.buckets.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes one request's worth from `key`'s bucket at `now_ms`.
    ///
    /// # Errors
    /// How many milliseconds until the next request would be allowed, when the bucket is empty.
    /// Counted in `hs_rate_limited_total{bucket}`.
    pub fn take(&self, key: &str, now_ms: u64) -> Result<(), u64> {
        self.check_and(key, now_ms, true)
    }

    /// Whether `key` could take a request at `now_ms`, without taking it: for a request that is
    /// only counted once it succeeds (a registration is counted when an account is made, not on
    /// every round of its user-interactive auth).
    ///
    /// # Errors
    /// As [`Self::take`].
    pub fn check(&self, key: &str, now_ms: u64) -> Result<(), u64> {
        self.check_and(key, now_ms, false)
    }

    /// [`Self::take`] at the current time.
    ///
    /// # Errors
    /// As [`Self::take`].
    pub fn take_now(&self, key: &str) -> Result<(), u64> {
        self.take(key, now_ms())
    }

    /// [`Self::check`] at the current time.
    ///
    /// # Errors
    /// As [`Self::take`].
    pub fn check_now(&self, key: &str) -> Result<(), u64> {
        self.check(key, now_ms())
    }

    fn check_and(&self, key: &str, now_ms: u64, consume: bool) -> Result<(), u64> {
        let Some(limit) = self.limit() else {
            return Ok(());
        };
        let burst = f64::from(limit.burst_count.max(1));
        let mut buckets = self.lock();
        if buckets.len() > PRUNE_ABOVE {
            buckets.retain(|_, bucket| {
                let elapsed = now_ms.saturating_sub(bucket.last_ms) as f64 / 1000.0;
                bucket.tokens + elapsed * limit.per_second < burst
            });
        }
        let bucket = buckets.entry(key.to_owned()).or_insert(KeyedBucket {
            tokens: burst,
            last_ms: now_ms,
        });
        let elapsed = now_ms.saturating_sub(bucket.last_ms) as f64 / 1000.0;
        // Clamped to the burst in force, so a lowered limit applies to what is left.
        bucket.tokens = (bucket.tokens + elapsed * limit.per_second).min(burst);
        bucket.last_ms = now_ms;
        if bucket.tokens >= 1.0 {
            if consume {
                bucket.tokens -= 1.0;
            }
            Ok(())
        } else {
            let wait = (1.0 - bucket.tokens) / limit.per_second * 1000.0;
            LIMITED
                .get_or_create(&LimitedLabels { bucket: self.name })
                .inc();
            tracing::debug!(bucket = self.name, key, "rate limited");
            // Small and positive here; the cast saturates rather than wraps.
            Err(wait.ceil() as u64)
        }
    }
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// A request extension saying the listener it arrived on sits behind a proxy whose
/// `X-Forwarded-For` is to be believed (the listener's `x_forwarded` setting).
#[derive(Debug, Clone, Copy, Default)]
pub struct TrustForwardedFor;

/// The address a request came from, for keying per-address rate limits: `None` when it cannot be
/// told apart from the server's own (see [`ClientIp::from_parts`]). Never refuses a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp(pub Option<IpAddr>);

impl ClientIp {
    /// The client's address, from what the listener recorded (`ConnectInfo<SocketAddr>`) and
    /// `X-Forwarded-For`:
    ///
    /// - The first address of `X-Forwarded-For`, when the listener trusts it
    ///   ([`TrustForwardedFor`]) or the peer is a loopback or private address -- a reverse proxy
    ///   or ingress in front of this server, whose own address would otherwise lump every client
    ///   into one bucket.
    /// - Otherwise the peer's address, unless it is a loopback address: a request from this host
    ///   with nothing forwarded is the operator's own tooling (`hs` itself, a health check, a
    ///   test), and is not limited per address.
    /// - `None` when there is no peer at all (a request replayed over the cluster mesh, or a
    ///   router driven in a test).
    #[must_use]
    pub fn from_parts(parts: &Parts) -> Self {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip());
        let trusted = parts.extensions.get::<TrustForwardedFor>().is_some()
            || peer.is_some_and(|ip| is_loopback(ip) || is_private(ip));
        if trusted
            && let Some(forwarded) = parts
                .headers
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next())
                .and_then(|first| first.trim().parse::<IpAddr>().ok())
        {
            return Self(Some(forwarded));
        }
        Self(peer.filter(|ip| !is_loopback(*ip)))
    }

    /// The address as a bucket key, if there is one.
    #[must_use]
    pub fn key(&self) -> Option<String> {
        self.0.map(|ip| ip.to_string())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_parts(parts))
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            // fc00::/7, unique local; and an IPv4-mapped private address.
            (v6.segments()[0] & 0xfe00) == 0xfc00
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_private() || v4.is_link_local())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(per_second: f64, burst_count: u32) -> Option<BucketLimit> {
        Some(BucketLimit {
            per_second,
            burst_count,
        })
    }

    #[test]
    fn no_limit_refuses_nothing() {
        let buckets = TokenBuckets::new("t");
        for _ in 0..100 {
            assert!(buckets.take("k", 0).is_ok());
        }
        buckets.set_limit(limit(0.0, 1));
        assert!(buckets.limit().is_none(), "a zero rate is no limit");
    }

    #[test]
    fn the_burst_then_the_rate_per_key() {
        let buckets = TokenBuckets::new("t");
        buckets.set_limit(limit(1.0, 2));
        assert!(buckets.take("a", 0).is_ok());
        assert!(buckets.take("a", 0).is_ok());
        assert_eq!(buckets.take("a", 0), Err(1000));
        assert!(
            buckets.take("b", 0).is_ok(),
            "another key has its own bucket"
        );
        assert!(buckets.take("a", 1000).is_ok());
    }

    #[test]
    fn a_check_takes_nothing() {
        let buckets = TokenBuckets::new("t");
        buckets.set_limit(limit(1.0, 1));
        assert!(buckets.check("a", 0).is_ok());
        assert!(buckets.check("a", 0).is_ok());
        assert!(buckets.take("a", 0).is_ok());
        assert!(buckets.check("a", 0).is_err());
    }

    #[test]
    fn lowering_the_limit_bites_at_once_and_removing_it_frees_everybody() {
        let buckets = TokenBuckets::new("t");
        buckets.set_limit(limit(1.0, 10));
        assert!(buckets.take("a", 0).is_ok());
        buckets.set_limit(limit(0.001, 1));
        assert!(
            buckets.take("a", 0).is_ok(),
            "one token left of the new burst"
        );
        assert!(buckets.take("a", 0).is_err());
        buckets.set_limit(None);
        assert!(buckets.take("a", 0).is_ok());
    }

    #[test]
    fn many_keys_are_pruned_once_they_refill() {
        let buckets = TokenBuckets::new("t");
        buckets.set_limit(limit(1000.0, 1));
        for key in 0..=PRUNE_ABOVE + 1 {
            assert!(buckets.take(&key.to_string(), 0).is_ok());
        }
        assert!(buckets.take("late", 10_000).is_ok());
        assert!(buckets.lock().len() < 10);
    }

    fn parts(peer: Option<&str>, forwarded: Option<&str>, trust: bool) -> Parts {
        let mut request = http::Request::builder().uri("/");
        if let Some(forwarded) = forwarded {
            request = request.header("x-forwarded-for", forwarded);
        }
        let (mut parts, ()) = request.body(()).unwrap().into_parts();
        if let Some(peer) = peer {
            parts
                .extensions
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        }
        if trust {
            parts.extensions.insert(TrustForwardedFor);
        }
        parts
    }

    #[test]
    fn the_client_address_follows_the_peer_and_a_trusted_proxy() {
        let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        // A public peer is the client; what it says it forwarded is not believed.
        assert_eq!(
            ClientIp::from_parts(&parts(Some("198.51.100.1:5"), Some("203.0.113.9"), false)).0,
            ip("198.51.100.1")
        );
        // Unless the listener says it sits behind a proxy.
        assert_eq!(
            ClientIp::from_parts(&parts(Some("198.51.100.1:5"), Some("203.0.113.9"), true)).0,
            ip("203.0.113.9")
        );
        // A private or loopback peer is a proxy: the first forwarded address is the client.
        assert_eq!(
            ClientIp::from_parts(&parts(
                Some("10.1.2.3:5"),
                Some("203.0.113.9, 10.0.0.1"),
                false
            ))
            .0,
            ip("203.0.113.9")
        );
        assert_eq!(
            ClientIp::from_parts(&parts(Some("127.0.0.1:5"), Some("203.0.113.9"), false)).0,
            ip("203.0.113.9")
        );
        // A private peer forwarding nothing is itself the client.
        assert_eq!(
            ClientIp::from_parts(&parts(Some("10.1.2.3:5"), None, false)).0,
            ip("10.1.2.3")
        );
        // This host's own tooling, and a request with no peer, are not keyed.
        assert_eq!(
            ClientIp::from_parts(&parts(Some("127.0.0.1:5"), None, false)).0,
            None
        );
        assert_eq!(
            ClientIp::from_parts(&parts(Some("[::1]:5"), None, false)).0,
            None
        );
        assert_eq!(ClientIp::from_parts(&parts(None, None, false)).0, None);
    }
}
