//! What this crate's write routes do for an account an administrator has acted on
//! (`hs-admin`'s `users.suspend`, `users.shadow_ban` and `users.rate_limit.*`):
//!
//! - **Suspended** (MSC3823, Matrix 1.14's account suspension): every write that puts
//!   something new in front of other people is refused with `403 M_USER_SUSPENDED` --
//!   sending, setting state, creating a room, joining, inviting, knocking, kicking, banning,
//!   unbanning, upgrading, adding an alias, and redacting anybody else's event. Leaving,
//!   forgetting, redacting one's own events and every read keep working, which is the point of
//!   suspension over locking: the person can still read and can still clean up after
//!   themself. [`refuse_if_suspended`].
//! - **Shadow-banned**: messages, state and redactions are answered with an event ID as if
//!   sent, and invitations with `{}` as if made, and none of it happens, so the person is not
//!   told and nobody else sees it (Synapse's shadow-ban, read for behavior only). Joining and
//!   leaving are not faked: those change what the person themself can see. [`shadow_event_id`].
//! - **Rate-limit override**: [`SendLimiter`] holds a token bucket per user with an override,
//!   filled from [`hs_auth::store::UserRecord::rate_limit_override`]; a sender over it is
//!   refused `429 M_LIMIT_EXCEEDED` with `retry_after_ms`. An override of `0` per second
//!   exempts the user. Without an override nothing is limited here: the server-wide
//!   `rate_limits.message` bucket has never been enforced by this crate, and turning it on is
//!   a separate decision (it changes the pace of every client and test).
//!
//! Every write refused, swallowed or throttled here is counted in
//! `hs_room_moderated_writes_total{outcome}` (`suspended`, `shadow_banned`, `rate_limited`),
//! which `hs serve` registers through [`register_metrics`]; the per-write logs are `debug`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use hs_auth::requester::Requester;
use hs_auth::store::RateLimitOverrideRecord;
use hs_kv::KvBackend;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use rand::Rng;
use ruma::{OwnedUserId, UserId};

use crate::error::RoomError;
use crate::state::RoomState;

/// The `outcome` label of `hs_room_moderated_writes_total`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, prometheus_client::encoding::EncodeLabelSet)]
struct ModeratedLabels {
    outcome: &'static str,
}

/// Process-wide, like the account flags it counts: the enforcement points are free functions
/// on the request path with no registry at hand, and a counter is only an atomic.
static MODERATED_WRITES: LazyLock<Family<ModeratedLabels, Counter>> =
    LazyLock::new(Family::default);

fn count(outcome: &'static str) {
    MODERATED_WRITES
        .get_or_create(&ModeratedLabels { outcome })
        .inc();
}

/// Registers `hs_room_moderated_writes_total{outcome}` into `registry`: writes this crate
/// refused from a suspended account (`suspended`), swallowed from a shadow-banned one
/// (`shadow_banned`) or throttled under a rate-limit override (`rate_limited`).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_room_moderated_writes",
        "Room writes refused, swallowed or throttled because of an administrator's moderation of \
         the account, by outcome: suspended, shadow_banned, rate_limited",
        MODERATED_WRITES.clone(),
    );
}

/// `403 M_USER_SUSPENDED` if the requester's account is suspended.
///
/// # Errors
/// [`RoomError::UserSuspended`].
pub(crate) fn refuse_if_suspended(requester: &Requester) -> Result<(), RoomError> {
    if requester.suspended {
        tracing::debug!(user = %requester.user_id, "refused a write from a suspended account");
        count("suspended");
        Err(RoomError::UserSuspended)
    } else {
        Ok(())
    }
}

/// An event ID that looks like one this server would mint (`$` and 43 URL-safe characters, the
/// room-version-3+ shape) and names no event, for answering a shadow-banned sender.
#[must_use]
pub fn shadow_event_id() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut rng = rand::rng();
    let body: String = (0..43)
        .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
        .collect();
    format!("${body}")
}

/// Logs a write that a shadow-ban swallowed. `debug`, not `info`: a shadow-banned spammer can
/// fill a log as fast as a room.
pub(crate) fn note_shadowed(requester: &Requester, what: &str) {
    tracing::debug!(user = %requester.user_id, what, "dropped a write from a shadow-banned account");
    count("shadow_banned");
}

struct Bucket {
    tokens: f64,
    last_ms: u64,
    per_second: f64,
    burst: f64,
}

/// Token buckets for the users who have a rate-limit override. In-process: in cluster mode each
/// replica limits what it handles, which with room-sharded routing is close to per-room. A
/// bucket is rebuilt when the override it was built from changes.
#[derive(Default)]
pub struct SendLimiter {
    buckets: Mutex<HashMap<OwnedUserId, Bucket>>,
}

impl SendLimiter {
    /// An empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes one event's worth from `user`'s bucket under `limit` at `now_ms`. `Err` carries how
    /// long until the next event would be allowed, in milliseconds. `limit: None` (no override)
    /// or `per_second == 0` (exempt) always allows, and forgets any bucket the user had.
    ///
    /// # Errors
    /// The wait, when the bucket is empty.
    pub fn check(
        &self,
        user: &UserId,
        limit: Option<RateLimitOverrideRecord>,
        now_ms: u64,
    ) -> Result<(), u64> {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(limit) = limit.filter(|l| l.per_second > 0.0) else {
            buckets.remove(user);
            return Ok(());
        };
        let burst = f64::from(limit.burst_count.max(1));
        let bucket = buckets.entry(user.to_owned()).or_insert(Bucket {
            tokens: burst,
            last_ms: now_ms,
            per_second: limit.per_second,
            burst,
        });
        #[allow(clippy::float_cmp)]
        if bucket.per_second != limit.per_second || bucket.burst != burst {
            *bucket = Bucket {
                tokens: burst,
                last_ms: now_ms,
                per_second: limit.per_second,
                burst,
            };
        }
        let elapsed = now_ms.saturating_sub(bucket.last_ms) as f64 / 1000.0;
        bucket.tokens = (bucket.tokens + elapsed * bucket.per_second).min(bucket.burst);
        bucket.last_ms = now_ms;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - bucket.tokens) / bucket.per_second * 1000.0;
            // The float is small and positive here; the cast saturates rather than wraps.
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

/// Applies the requester's rate-limit override, if they have one, to one event about to be
/// sent. An appservice the registry exempts from rate limiting is never limited here.
///
/// # Errors
/// [`RoomError::LimitExceeded`] when over the limit; [`RoomError::Internal`] if the account
/// record cannot be read.
pub(crate) async fn check_send_limit<B: KvBackend + 'static>(
    state: &RoomState<B>,
    requester: &Requester,
) -> Result<(), RoomError> {
    if requester
        .appservice
        .as_ref()
        .is_some_and(|a| !a.rate_limited)
    {
        return Ok(());
    }
    let record = state
        .auth
        .store
        .get_user(&requester.user_id)
        .await
        .map_err(|e| RoomError::Internal(format!("reading the sender's account: {e}")))?;
    let limit = record.and_then(|r| r.rate_limit_override);
    state
        .rooms
        .send_limiter()
        .check(&requester.user_id, limit, now_ms())
        .map_err(|retry_after_ms| {
            tracing::debug!(user = %requester.user_id, retry_after_ms, "a sender is over their rate-limit override");
            count("rate_limited");
            RoomError::LimitExceeded(retry_after_ms)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::user_id;

    #[test]
    fn a_shadow_event_id_looks_like_a_real_one() {
        let id = shadow_event_id();
        assert!(ruma::EventId::parse(&id).is_ok(), "{id}");
        assert_eq!(id.len(), 44);
        assert_ne!(id, shadow_event_id());
    }

    #[test]
    fn the_bucket_allows_the_burst_then_the_rate() {
        let limiter = SendLimiter::new();
        let alice = user_id!("@alice:example.org");
        let limit = Some(RateLimitOverrideRecord {
            per_second: 2.0,
            burst_count: 2,
        });
        assert!(limiter.check(alice, limit, 1_000).is_ok());
        assert!(limiter.check(alice, limit, 1_000).is_ok());
        let wait = limiter.check(alice, limit, 1_000).unwrap_err();
        assert_eq!(wait, 500);
        assert!(limiter.check(alice, limit, 1_500).is_ok());
        // Nobody else shares alice's bucket.
        assert!(
            limiter
                .check(user_id!("@bob:example.org"), limit, 1_500)
                .is_ok()
        );
    }

    #[test]
    fn no_override_or_zero_is_unlimited_and_a_changed_override_starts_afresh() {
        let limiter = SendLimiter::new();
        let alice = user_id!("@alice:example.org");
        for _ in 0..100 {
            assert!(limiter.check(alice, None, 0).is_ok());
            assert!(
                limiter
                    .check(
                        alice,
                        Some(RateLimitOverrideRecord {
                            per_second: 0.0,
                            burst_count: 1,
                        }),
                        0,
                    )
                    .is_ok()
            );
        }
        let strict = Some(RateLimitOverrideRecord {
            per_second: 0.1,
            burst_count: 1,
        });
        assert!(limiter.check(alice, strict, 0).is_ok());
        assert!(limiter.check(alice, strict, 0).is_err());
        let looser = Some(RateLimitOverrideRecord {
            per_second: 0.1,
            burst_count: 5,
        });
        assert!(limiter.check(alice, looser, 0).is_ok());
    }
}
