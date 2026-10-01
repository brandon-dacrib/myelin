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
//!   exempts the user.
//! - **The server-wide limit**: everybody without an override sends under the configuration's
//!   `rate_limits.message`, which `hs serve` hands [`SendLimiter::set_server_limit`] at startup
//!   and again whenever an operator changes it (decision 0016). Without it -- a room layer
//!   nobody configured, as in this crate's own tests -- nobody without an override is limited.
//!
//! Every write refused, swallowed or throttled here because of moderation is counted in
//! `hs_room_moderated_writes_total{outcome}` (`suspended`, `shadow_banned`, `rate_limited`), and
//! every write refused under the server-wide limit in `hs_room_server_rate_limited_writes_total`;
//! `hs serve` registers both through [`register_metrics`]. The per-write logs are `debug`.

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

/// Writes refused under the server-wide `rate_limits.message`, process-wide for the same reason.
static SERVER_RATE_LIMITED: LazyLock<Counter> = LazyLock::new(Counter::default);

/// Guest accounts refused a room because its `m.room.guest_access` does not let guests in.
static GUEST_JOINS_REFUSED: LazyLock<Counter> = LazyLock::new(Counter::default);

/// Counts one guest refused a room by its guest access (`hs_room_guest_joins_refused_total`).
pub(crate) fn count_guest_join_refused() {
    GUEST_JOINS_REFUSED.inc();
}

fn count(outcome: &'static str) {
    MODERATED_WRITES
        .get_or_create(&ModeratedLabels { outcome })
        .inc();
}

/// Registers `hs_room_moderated_writes_total{outcome}` into `registry` -- writes this crate
/// refused from a suspended account (`suspended`), swallowed from a shadow-banned one
/// (`shadow_banned`) or throttled under a rate-limit override (`rate_limited`) -- and
/// `hs_room_server_rate_limited_writes_total`, writes refused under the server-wide limit, and
/// `hs_room_guest_joins_refused_total`, guests refused a room by its guest access.
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    // Registered without `_total`: the text encoder appends it.
    registry.register(
        "hs_room_moderated_writes",
        "Room writes refused, swallowed or throttled because of an administrator's moderation of \
         the account, by outcome: suspended, shadow_banned, rate_limited",
        MODERATED_WRITES.clone(),
    );
    registry.register(
        "hs_room_server_rate_limited_writes",
        "Room writes refused 429 under the server-wide rate_limits.message limit",
        SERVER_RATE_LIMITED.clone(),
    );
    registry.register(
        "hs_room_guest_joins_refused",
        "Joins by guest accounts refused 403 because the room's m.room.guest_access is not \
         can_join",
        GUEST_JOINS_REFUSED.clone(),
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

/// Which limit a user's bucket was filled under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    /// The administrator's per-user override (`users.rate_limit.*`).
    Override,
    /// The server-wide `rate_limits.message`, for everybody without an override.
    Server,
}

struct Bucket {
    tokens: f64,
    last_ms: u64,
    per_second: f64,
    burst: f64,
    source: LimitSource,
}

/// Token buckets for senders: under an administrator's per-user override when there is one,
/// otherwise under the server-wide limit ([`SendLimiter::set_server_limit`], the configuration's
/// `rate_limits.message`), which is swapped in while the server runs. In-process: in cluster
/// mode each replica limits what it handles, which with room-sharded routing is close to
/// per-room.
///
/// A bucket starts afresh, full, when the user gains or loses an override or their override
/// changes. When the server-wide limit changes, a bucket under it keeps what it has left,
/// clamped to the new burst: lowering the limit takes effect at once rather than handing every
/// sender a fresh burst first.
#[derive(Default)]
pub struct SendLimiter {
    buckets: Mutex<HashMap<OwnedUserId, Bucket>>,
    server: std::sync::RwLock<Option<RateLimitOverrideRecord>>,
}

impl SendLimiter {
    /// An empty limiter, with no server-wide limit.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the server-wide limit for senders without an override. `None`, or a
    /// `per_second` of `0`, limits nobody. Takes effect on the next event anybody sends.
    pub fn set_server_limit(&self, limit: Option<RateLimitOverrideRecord>) {
        *self
            .server
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = limit;
    }

    /// The server-wide limit in force.
    #[must_use]
    pub fn server_limit(&self) -> Option<RateLimitOverrideRecord> {
        *self
            .server
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes one event's worth from `user`'s bucket at `now_ms`, under `limit` (their override)
    /// or, without one, under the server-wide limit. `Err` carries how long until the next event
    /// would be allowed, in milliseconds. An override with `per_second == 0` (exempt), or no
    /// limit at all, always allows and forgets any bucket the user had.
    ///
    /// # Errors
    /// The wait, when the bucket is empty.
    pub fn check(
        &self,
        user: &UserId,
        limit: Option<RateLimitOverrideRecord>,
        now_ms: u64,
    ) -> Result<(), u64> {
        self.take(user, limit, now_ms).map_err(|(wait, _)| wait)
    }

    /// [`Self::check`], saying which limit refused.
    ///
    /// # Errors
    /// The wait, and whether the override or the server-wide limit refused.
    pub fn take(
        &self,
        user: &UserId,
        limit: Option<RateLimitOverrideRecord>,
        now_ms: u64,
    ) -> Result<(), (u64, LimitSource)> {
        let (limit, source) = match limit {
            Some(own) => (Some(own), LimitSource::Override),
            None => (self.server_limit(), LimitSource::Server),
        };
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
            source,
        });
        #[allow(clippy::float_cmp)]
        let changed = bucket.per_second != limit.per_second || bucket.burst != burst;
        if bucket.source != source || (changed && source == LimitSource::Override) {
            *bucket = Bucket {
                tokens: burst,
                last_ms: now_ms,
                per_second: limit.per_second,
                burst,
                source,
            };
        } else if changed {
            // The server-wide limit moved under this bucket. When is not known (the change is
            // seen on this sender's next event), so the time since their last one refills at
            // the new rate, below, and what they had is kept up to the new burst.
            bucket.tokens = bucket.tokens.min(burst);
            bucket.per_second = limit.per_second;
            bucket.burst = burst;
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
            Err((wait.ceil() as u64, source))
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
        .take(&requester.user_id, limit, now_ms())
        .map_err(|(retry_after_ms, source)| {
            match source {
                LimitSource::Override => {
                    tracing::debug!(user = %requester.user_id, retry_after_ms, "a sender is over their rate-limit override");
                    count("rate_limited");
                }
                LimitSource::Server => {
                    tracing::debug!(user = %requester.user_id, retry_after_ms, "a sender is over the server-wide rate limit");
                    SERVER_RATE_LIMITED.inc();
                }
            }
            RoomError::LimitExceeded(retry_after_ms)
        })
}

/// `rate_limits.joins_local` or `rate_limits.joins_remote` (`remote`: the join goes through
/// another server), per user, for one join about to be made: the server-wide buckets in
/// [`hs_auth::ratelimit::ServerLimits`], whose limits a running server replaces when they change.
/// An appservice the registry exempts from rate limiting is never limited here.
///
/// # Errors
/// [`RoomError::LimitExceeded`] when the user's bucket is empty.
pub(crate) fn check_join_limit<B: KvBackend + 'static>(
    state: &RoomState<B>,
    requester: &Requester,
    remote: bool,
) -> Result<(), RoomError> {
    if requester
        .appservice
        .as_ref()
        .is_some_and(|a| !a.rate_limited)
    {
        return Ok(());
    }
    let limits = &state.auth.limits;
    let buckets = if remote {
        &limits.joins_remote
    } else {
        &limits.joins_local
    };
    buckets
        .take_now(requester.user_id.as_str())
        .map_err(RoomError::LimitExceeded)
}

/// The limit on one redaction about to be sent: a server administrator's redactions are under
/// `rate_limits.admin_redaction` (as Synapse's `rc_admin_redaction`) unless an administrator
/// gave them an override; everybody else's under the send limit ([`check_send_limit`]).
///
/// # Errors
/// As [`check_send_limit`].
pub(crate) async fn check_redaction_limit<B: KvBackend + 'static>(
    state: &RoomState<B>,
    requester: &Requester,
) -> Result<(), RoomError> {
    if !requester.is_admin
        || requester
            .appservice
            .as_ref()
            .is_some_and(|a| !a.rate_limited)
    {
        return check_send_limit(state, requester).await;
    }
    let record = state
        .auth
        .store
        .get_user(&requester.user_id)
        .await
        .map_err(|e| RoomError::Internal(format!("reading the sender's account: {e}")))?;
    if record.is_some_and(|r| r.rate_limit_override.is_some()) {
        return check_send_limit(state, requester).await;
    }
    state
        .auth
        .limits
        .admin_redaction
        .take_now(requester.user_id.as_str())
        .map_err(RoomError::LimitExceeded)
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

    fn limit(per_second: f64, burst_count: u32) -> Option<RateLimitOverrideRecord> {
        Some(RateLimitOverrideRecord {
            per_second,
            burst_count,
        })
    }

    #[test]
    fn without_an_override_the_server_limit_applies_and_says_so() {
        let limiter = SendLimiter::new();
        let alice = user_id!("@alice:example.org");
        limiter.set_server_limit(limit(1.0, 2));
        assert!(limiter.take(alice, None, 0).is_ok());
        assert!(limiter.take(alice, None, 0).is_ok());
        assert_eq!(
            limiter.take(alice, None, 0),
            Err((1_000, LimitSource::Server))
        );
        // An override outranks it, in both directions: exempt...
        assert!(limiter.take(alice, limit(0.0, 1), 0).is_ok());
        // ...or stricter, and the refusal names the override.
        assert!(limiter.take(alice, limit(0.5, 1), 0).is_ok());
        assert_eq!(
            limiter.take(alice, limit(0.5, 1), 0),
            Err((2_000, LimitSource::Override))
        );
        // Cleared, the server limit is back with a full bucket of its own.
        assert!(limiter.take(alice, None, 0).is_ok());
    }

    #[test]
    fn lowering_the_server_limit_takes_effect_at_once_and_keeps_what_is_left() {
        let limiter = SendLimiter::new();
        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        limiter.set_server_limit(limit(0.2, 10));
        for _ in 0..8 {
            assert!(limiter.take(alice, None, 0).is_ok());
        }
        // Alice has two left. Lowered to a burst of one, she keeps one, not a fresh burst.
        limiter.set_server_limit(limit(0.01, 1));
        assert_eq!(limiter.server_limit(), limit(0.01, 1));
        assert!(limiter.take(alice, None, 0).is_ok());
        assert_eq!(
            limiter.take(alice, None, 0),
            Err((100_000, LimitSource::Server))
        );
        // Somebody who never sent starts with the new burst.
        assert!(limiter.take(bob, None, 0).is_ok());
        assert!(limiter.take(bob, None, 0).is_err());
        // Raised again, what she has left refills at the new rate from here.
        limiter.set_server_limit(limit(10.0, 5));
        assert!(limiter.take(alice, None, 100).is_ok());
        // Switched off, nobody is limited and the buckets are forgotten.
        limiter.set_server_limit(None);
        for _ in 0..50 {
            assert!(limiter.take(alice, None, 100).is_ok());
        }
    }
}
