//! Rate limits: one token-bucket per limited action. Corresponds to
//! Synapse's `rc_*` family (`rc_message`, `rc_registration`, `rc_login`,
//! `rc_joins`, `rc_admin_redaction`, `rc_federation`). Hot-reloadable (see
//! [`crate::reload`]): a change applies to the running server at once.
//!
//! Every bucket is enforced (decision 0016's 2026-10-01 amendment): a request over its limit is
//! refused with `429 M_LIMIT_EXCEEDED` and `retry_after_ms`, which clients wait out and retry.
//! `message` is counted per sender (an administrator's per-user override replaces it for that
//! user), `login` and `registration` per client address, the joins per user, `federation` per
//! origin server. In a cluster every bucket is counted by each replica on its own.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};

/// A token-bucket rate limit: `burst_count` tokens refilling at
/// `per_second` tokens/second.
///
/// # A bucket written in part
///
/// Each named bucket on [`RateLimitConfig`] (`message`, `login`, ...) has its
/// own default values for the two fields, which a per-field
/// `serde(default = ...)` cannot express. So each bucket field of
/// [`RateLimitConfig`] is read through its own function (`partial_message`,
/// ...) that fills a field left out with *that bucket's* default. A bucket can
/// therefore be written in part anywhere: `HS__RATE_LIMITS__LOGIN__PER_SECOND`
/// alone, or a `config.update` of just `rate_limits.message.burst_count`
/// (which the management interface sends when one field is edited), keeps
/// the bucket's own default for the other field.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitBucket {
    /// How fast the bucket refills, in actions per second, once its burst is spent. May be
    /// fractional: `0.2` is one every five seconds, `0.01` one every hundred seconds. Lower is
    /// stricter; `0` switches this one limit off.
    pub per_second: f64,
    /// How many actions may happen back to back before the refill rate applies. A person rarely
    /// does more than a few at once, so a small burst stops scripts without anyone noticing it.
    /// At least 1.
    pub burst_count: u32,
}

impl RateLimitBucket {
    const fn new(per_second: f64, burst_count: u32) -> Self {
        Self {
            per_second,
            burst_count,
        }
    }
}

const fn default_true() -> bool {
    true
}

fn default_message() -> RateLimitBucket {
    RateLimitBucket::new(0.2, 10)
}
fn default_registration() -> RateLimitBucket {
    RateLimitBucket::new(0.17, 3)
}
fn default_login() -> RateLimitBucket {
    RateLimitBucket::new(0.17, 3)
}
fn default_joins_local() -> RateLimitBucket {
    RateLimitBucket::new(0.1, 10)
}
fn default_joins_remote() -> RateLimitBucket {
    RateLimitBucket::new(0.01, 10)
}
fn default_admin_redaction() -> RateLimitBucket {
    RateLimitBucket::new(1.0, 50)
}
fn default_federation() -> RateLimitBucket {
    RateLimitBucket::new(10.0, 100)
}
fn default_third_party_id_validation() -> RateLimitBucket {
    RateLimitBucket::new(0.003, 5)
}

/// A bucket as written: either field may be left out.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialBucket {
    per_second: Option<f64>,
    burst_count: Option<u32>,
}

/// One `partial_<bucket>` function per bucket: reads a bucket written in part, filling what is
/// left out from that bucket's own default (see [`RateLimitBucket`]).
macro_rules! partial_bucket {
    ($($name:ident => $default:ident),* $(,)?) => {
        $(
            fn $name<'de, D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> Result<RateLimitBucket, D::Error> {
                let written = PartialBucket::deserialize(deserializer)?;
                let default = $default();
                Ok(RateLimitBucket {
                    per_second: written.per_second.unwrap_or(default.per_second),
                    burst_count: written.burst_count.unwrap_or(default.burst_count),
                })
            }
        )*
    };
}

partial_bucket! {
    partial_message => default_message,
    partial_registration => default_registration,
    partial_login => default_login,
    partial_joins_local => default_joins_local,
    partial_joins_remote => default_joins_remote,
    partial_admin_redaction => default_admin_redaction,
    partial_federation => default_federation,
    partial_third_party_id_validation => default_third_party_id_validation,
}

/// Rate limits: how fast one user, address or server may do each costly thing before requests
/// are refused with `429` and a time to wait. Defaults are Synapse's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Whether any rate limit is enforced. Leave it on: switched off, nothing stops one account
    /// or address from flooding rooms, guessing passwords or registering accounts in bulk. It
    /// exists for test harnesses and benchmarks that send far faster than people do.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How fast one user may send events: messages, room settings (state) and redactions,
    /// counted per sender. An administrator can give one user other limits on their user page.
    /// Corresponds to Synapse's `rc_message`. Bridges that
    /// registered with `rate_limited: false` are exempt.
    #[serde(default = "default_message", deserialize_with = "partial_message")]
    pub message: RateLimitBucket,
    /// How fast accounts may be created from one client address (`POST /register`). Counted when
    /// an account is actually made, so a sign-up that fails a step costs nothing. Keeps a script
    /// from registering accounts in bulk when registration is open. Bridges are exempt.
    /// Corresponds to Synapse's `rc_registration`.
    #[serde(
        default = "default_registration",
        deserialize_with = "partial_registration"
    )]
    pub registration: RateLimitBucket,
    /// How fast sign-in attempts may come from one client address (`POST /login`): the guard
    /// against password guessing. The address is the first one a proxy in front of the server
    /// forwards, so set the listener's `x_forwarded` behind a public proxy. Bridges are exempt.
    /// Corresponds to Synapse's `rc_login.address`.
    #[serde(default = "default_login", deserialize_with = "partial_login")]
    pub login: RateLimitBucket,
    /// How fast one user may join rooms this server already takes part in. Joins are cheap here,
    /// so the limit only stops a script joining hundreds of rooms at once. Corresponds to
    /// Synapse's `rc_joins.local`.
    #[serde(
        default = "default_joins_local",
        deserialize_with = "partial_joins_local"
    )]
    pub joins_local: RateLimitBucket,
    /// How fast one user may join rooms on other servers. Each such join fetches the room's state
    /// over federation, which can take this server minutes and a lot of memory for a large
    /// room, so this limit is the strictest. Corresponds to Synapse's `rc_joins.remote`.
    #[serde(
        default = "default_joins_remote",
        deserialize_with = "partial_joins_remote"
    )]
    pub joins_remote: RateLimitBucket,
    /// How fast a server administrator's redactions may go, in place of the message limit, so
    /// that removing a spammer's messages is not throttled like ordinary sending. Corresponds to
    /// Synapse's `rc_admin_redaction`.
    #[serde(
        default = "default_admin_redaction",
        deserialize_with = "partial_admin_redaction"
    )]
    pub admin_redaction: RateLimitBucket,
    /// How fast one other server may send transactions to this one (`PUT /send`), counted per
    /// origin server. A server catching up after an outage sends in bursts; too low a limit
    /// slows how soon its users' messages arrive here. Corresponds to Synapse's
    /// `rc_federation`.
    #[serde(
        default = "default_federation",
        deserialize_with = "partial_federation"
    )]
    pub federation: RateLimitBucket,
    /// How fast validation emails may be requested (`POST /register/email/requestToken`,
    /// `/account/3pid/email/requestToken`, `/account/password/email/requestToken`), counted per
    /// client address and per email address, so nobody can use this server to flood a mailbox.
    /// Corresponds to Synapse's `rc_3pid_validation`.
    #[serde(
        default = "default_third_party_id_validation",
        deserialize_with = "partial_third_party_id_validation"
    )]
    pub third_party_id_validation: RateLimitBucket,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            message: default_message(),
            registration: default_registration(),
            login: default_login(),
            joins_local: default_joins_local(),
            joins_remote: default_joins_remote(),
            admin_redaction: default_admin_redaction(),
            federation: default_federation(),
            third_party_id_validation: default_third_party_id_validation(),
        }
    }
}

fn validate_bucket(prefix: &str, field: &str, b: &RateLimitBucket, errors: &mut ValidationErrors) {
    if !b.per_second.is_finite() || b.per_second < 0.0 {
        errors.push(
            format!("{prefix}.{field}.per_second"),
            format!("must be a non-negative finite number, got {}", b.per_second),
        );
    }
    if b.burst_count == 0 {
        errors.push(
            format!("{prefix}.{field}.burst_count"),
            "must be at least 1",
        );
    }
}

impl Validate for RateLimitConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        validate_bucket(prefix, "message", &self.message, errors);
        validate_bucket(prefix, "registration", &self.registration, errors);
        validate_bucket(prefix, "login", &self.login, errors);
        validate_bucket(prefix, "joins_local", &self.joins_local, errors);
        validate_bucket(prefix, "joins_remote", &self.joins_remote, errors);
        validate_bucket(prefix, "admin_redaction", &self.admin_redaction, errors);
        validate_bucket(prefix, "federation", &self.federation, errors);
        validate_bucket(
            prefix,
            "third_party_id_validation",
            &self.third_party_id_validation,
            errors,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let mut errors = ValidationErrors::new();
        RateLimitConfig::default().validate("rate_limits", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn rejects_negative_rate() {
        let mut cfg = RateLimitConfig::default();
        cfg.login.per_second = -1.0;
        let mut errors = ValidationErrors::new();
        cfg.validate("rate_limits", &mut errors);
        assert_eq!(errors.0[0].path, "rate_limits.login.per_second");
    }

    #[test]
    fn rejects_nan_rate() {
        let mut cfg = RateLimitConfig::default();
        cfg.message.per_second = f64::NAN;
        let mut errors = ValidationErrors::new();
        cfg.validate("rate_limits", &mut errors);
        assert_eq!(errors.0.len(), 1);
    }

    #[test]
    fn rejects_zero_burst() {
        let mut cfg = RateLimitConfig::default();
        cfg.registration.burst_count = 0;
        let mut errors = ValidationErrors::new();
        cfg.validate("rate_limits", &mut errors);
        assert_eq!(errors.0[0].path, "rate_limits.registration.burst_count");
    }
}
