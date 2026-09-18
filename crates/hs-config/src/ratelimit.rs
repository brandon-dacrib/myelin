//! Rate limits: one token-bucket per limited action. Corresponds to
//! Synapse's `rc_*` family (`rc_message`, `rc_registration`, `rc_login`,
//! `rc_joins`, `rc_admin_redaction`, `rc_federation`). Hot-reloadable (see
//! [`crate::reload`]).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};

/// A token-bucket rate limit: `burst_count` tokens refilling at
/// `per_second` tokens/second.
///
/// # A note on partial environment overrides
///
/// Unlike most structs in this crate, `per_second` and `burst_count` have
/// no individual `serde(default = ...)`: each named bucket on
/// [`RateLimitConfig`] (`message`, `login`, ...) has its own default values
/// for the two fields, and a per-field default function has no way to know
/// which bucket it is filling in. The practical effect: `HS__` overrides
/// that set only one field of a bucket (`HS__RATE_LIMITS__LOGIN__PER_SECOND`)
/// require the base config to already spell out that bucket in full — the
/// override then replaces just the one field, keeping the file's own value
/// for the other. Overriding a field of a bucket that is entirely absent
/// from the base config is a parse error (a clear "missing field", not a
/// silently wrong bucket).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitBucket {
    /// Steady-state rate, in actions per second. May be fractional (e.g.
    /// `0.17` is roughly one every six seconds, Synapse's `rc_message`
    /// default).
    pub per_second: f64,
    /// Bucket size: how many actions may happen back-to-back before the
    /// steady-state rate applies.
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

/// All configured rate-limit buckets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Master switch; when false, no limiter runs (tests and benchmarking
    /// only — never recommended in production).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-user event sending. Corresponds to Synapse's `rc_message`.
    #[serde(default = "default_message")]
    pub message: RateLimitBucket,
    /// `POST /register`. Corresponds to Synapse's `rc_registration`.
    #[serde(default = "default_registration")]
    pub registration: RateLimitBucket,
    /// `POST /login`. Corresponds to Synapse's `rc_login.address`.
    #[serde(default = "default_login")]
    pub login: RateLimitBucket,
    /// Local room joins. Corresponds to Synapse's `rc_joins.local`.
    #[serde(default = "default_joins_local")]
    pub joins_local: RateLimitBucket,
    /// Joins to rooms on remote servers. Corresponds to Synapse's
    /// `rc_joins.remote`.
    #[serde(default = "default_joins_remote")]
    pub joins_remote: RateLimitBucket,
    /// Admin-triggered redactions. Corresponds to Synapse's
    /// `rc_admin_redaction`.
    #[serde(default = "default_admin_redaction")]
    pub admin_redaction: RateLimitBucket,
    /// Inbound federation transactions per origin server. Corresponds to
    /// Synapse's `rc_federation`.
    #[serde(default = "default_federation")]
    pub federation: RateLimitBucket,
    /// `POST /account/3pid/*/requestToken`. Corresponds to Synapse's
    /// `rc_3pid_validation`.
    #[serde(default = "default_third_party_id_validation")]
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
