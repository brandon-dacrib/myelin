//! Configuration for pluggable content adaptation (`docs/rfcs/0008-content-scanning.md`).
//!
//! This type deliberately does **not** live in `hs_config::MediaConfig` (track 13's crate, which
//! this track may not edit — see `docs/status/09-media.md`'s ownership rule). Instead it is a
//! self-contained, independently deserializable section (`media.scanning` in the operator's YAML)
//! that whoever assembles the real `hs_config::Config` document merges in. This mirrors how RFC
//! 0006 (URL previews) already left new `MediaConfig` fields as a proposal rather than an edit.
//!
//! Reuses `hs_config::{Duration, ByteSize}` for parse compatibility with the rest of the config
//! system (`"30s"`, `"100MiB"`, ...) and `hs_config::Validate` for the same
//! `path -> message` validation-error shape every other config section uses.
//!
//! # Scope cut: only `icap`, `http` and `none` (decision 0007)
//!
//! An earlier draft of this module also had `ProviderKind::ClamAv` (a direct clamd client) and
//! `ProviderKind::Command` (spawn-a-binary). Both were deleted before any client code was written
//! for either, per `docs/decisions/0007-build-less-reuse-more.md`: c-icap's `virus_scan` service
//! already drives ClamAV, with packaged container images, so a second, homegrown path to the same
//! engine is exactly the duplicated-maintenance-forever the decision rules out. An operator who
//! wants ClamAV runs c-icap in front of it (see `deploy/`) and configures `provider: icap`.

use hs_config::error::{Validate, ValidationErrors};
use hs_config::{ByteSize, Duration};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::MediaError;

/// When scanning happens relative to the upload becoming servable (RFC section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    /// The upload fails until a verdict arrives. Safest; adds upload latency.
    Block,
    /// The upload succeeds but the media is not retrievable (`404`, indistinguishable from
    /// unknown media) until a verdict arrives.
    Defer,
    /// The media is served immediately; quarantined after the fact if the verdict is bad.
    Quarantine,
    /// No scanning. The default.
    #[default]
    Off,
}

/// What happens when the scanner cannot produce a verdict in time (timeout, connection refused,
/// malformed response). Not defaulted: see [`ScanningConfig::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailPolicy {
    /// Treat scanner errors and timeouts as rejection.
    Closed,
    /// Allow the upload through and record the failure.
    Open,
}

/// A tri-state action, used for [`ScanningConfig::oversize`] and
/// [`UnscannablePolicy`]'s per-reason actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Let the content through as if it were clean.
    Allow,
    /// Reject the upload (or, in quarantine mode, quarantine it) as if it were infected.
    Block,
    /// Serve it, but quarantine so an admin must explicitly clear it.
    Quarantine,
}

/// Which provider adapter to use. See `crate::scanning::providers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// No scanning (the default). `mode` is ignored if set to anything but `off` — see
    /// [`ScanningConfig::validate`].
    #[default]
    None,
    /// ICAP RESPMOD (RFC 3507). The provider: c-icap (fronting ClamAV or anything else),
    /// commercial engines with native ICAP interfaces, and cloud gateways such as ICAPeg all
    /// reach us through this one adapter. See `crate::scanning::providers::icap`.
    Icap,
    /// The JSON submit-and-poll HTTP contract, for cloud APIs with no ICAP fronting (notably
    /// CrowdStrike Falcon). See `crate::scanning::providers::http`.
    Http,
}

/// How the client negotiates ICAP preview mode. `negotiate`/`off` serialize as bare strings;
/// a forced size serializes as `{bytes: N}` (serde's default externally-tagged representation for
/// a unit vs. tuple variant), matching the RFC's `preview: negotiate` example.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum PreviewMode {
    /// Follow the server's `OPTIONS`-advertised `Transfer-Preview`/`Transfer-Ignore`/
    /// `Transfer-Complete` policy (the default, and what real ICAP services such as c-icap
    /// expect an operator to rely on).
    #[default]
    Negotiate,
    /// Force a specific preview size regardless of what `OPTIONS` advertises.
    Bytes(usize),
    /// Never preview; always send the complete body.
    Off,
}

/// `icap` provider settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IcapConfig {
    /// ICAP server host.
    pub host: String,
    /// ICAP server port (default 1344).
    #[serde(default = "default_icap_port")]
    pub port: u16,
    /// The ICAP service name, e.g. `virus_scan` (c-icap's ClamAV service alias) or `avscan`.
    pub service: String,
    /// Preview negotiation.
    #[serde(default)]
    pub preview: PreviewMode,
}

fn default_icap_port() -> u16 {
    1344
}

/// `http` provider settings: the submit endpoint, and an optional separate poll endpoint for
/// providers that answer asynchronously (RFC section 3, CrowdStrike Falcon's submit-then-poll
/// API).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// `POST` here with the scan submission.
    pub submit_url: String,
    /// `GET <poll_url>/<ticket>` to poll a pending scan. Defaults to `submit_url` (some
    /// providers poll the same endpoint with a `?ticket=` query, which this crate's client
    /// appends automatically when `poll_url` is unset).
    #[serde(default)]
    pub poll_url: Option<String>,
    /// A bearer token or API key sent as `Authorization: Bearer <token>`, if set.
    #[serde(default)]
    pub auth_token: Option<String>,
}

/// The verdict cache's knobs (RFC section 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// How long a verdict is cached when the provider reports an `engine_version`.
    #[serde(default = "default_cache_ttl")]
    pub ttl: Duration,
    /// How long a verdict is cached when the provider cannot report an `engine_version`
    /// (`ContentScanner::engine_version` returned `None`).
    #[serde(default = "default_unversioned_ttl")]
    pub unversioned_ttl: Duration,
    /// Maximum number of cached verdicts. Oldest-inserted entries are evicted first once
    /// exceeded.
    #[serde(default = "default_cache_capacity")]
    pub capacity: usize,
}

fn default_cache_ttl() -> Duration {
    Duration::from_days(7)
}

fn default_unversioned_ttl() -> Duration {
    Duration::from_hours(1)
}

fn default_cache_capacity() -> usize {
    100_000
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl: default_cache_ttl(),
            unversioned_ttl: default_unversioned_ttl(),
            capacity: default_cache_capacity(),
        }
    }
}

/// The policy applied to each [`crate::scanning::types::UnscannableReason`]. RFC section 7:
/// encrypted media defaults to allow ("blocking it disables encrypted media entirely"); every
/// other unscannable reason defaults to the more conservative block, since — unlike encryption —
/// those reasons (archive too deep, oversized, unrecognized format) are not an inherent property
/// of end-to-end encryption and an operator may want them investigated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnscannablePolicy {
    /// Action for [`crate::scanning::types::UnscannableReason::Encrypted`]. Default: allow.
    #[serde(default = "default_allow")]
    pub encrypted: Action,
    /// Action for every other unscannable reason (`TooDeep`, `UnsupportedFormat`, `Other`).
    /// `TooLarge` is governed by [`ScanningConfig::oversize`] instead, not this field. Default:
    /// block.
    #[serde(default = "default_block")]
    pub other: Action,
}

fn default_allow() -> Action {
    Action::Allow
}

fn default_block() -> Action {
    Action::Block
}

impl Default for UnscannablePolicy {
    fn default() -> Self {
        Self {
            encrypted: default_allow(),
            other: default_block(),
        }
    }
}

/// Per-appservice scanning bypass (RFC section 4: "a bridge may be configured to bypass only by
/// explicit per-appservice configuration, which is recorded in the audit log").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppserviceBypass {
    /// Appservice registration IDs (`hs_auth::requester::AppserviceIdentity::appservice_id`)
    /// exempt from scanning. Every use of this list is written to the audit log — see
    /// `crate::scanning::audit`.
    #[serde(default)]
    pub exempt_appservice_ids: Vec<String>,
}

/// Top-level scanning/adaptation configuration (`media.scanning` in the operator's YAML).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanningConfig {
    /// When scanning happens. `Off` disables scanning entirely, regardless of `provider`.
    #[serde(default)]
    pub mode: ScanMode,
    /// Which provider adapter to use.
    #[serde(default)]
    pub provider: ProviderKind,
    /// Fail-open or fail-closed on scanner error/timeout. **Not defaulted**: `None` is only
    /// valid when `mode == Off`; enabling scanning without choosing this is a configuration
    /// error (RFC section 5). Kept as `Option` specifically so serde does not silently supply a
    /// value the operator never chose.
    #[serde(default)]
    pub fail: Option<FailPolicy>,
    /// Whether a service may return a modified body (RFC section 3.4). Off by default: an
    /// operator who has not asked for content rewriting must never silently get it. When
    /// `false`, a provider that returns adapted content is treated as a scanner error under
    /// `fail` (see `crate::scanning::engine`).
    #[serde(default)]
    pub allow_replacement: bool,
    /// Per-scan deadline.
    #[serde(default = "default_timeout")]
    pub timeout: Duration,
    /// Content above this size is treated per `oversize` instead of being scanned.
    #[serde(default = "default_max_size")]
    pub max_size: ByteSize,
    /// What to do with content over `max_size`.
    #[serde(default = "default_oversize")]
    pub oversize: Action,
    /// The verdict cache.
    #[serde(default)]
    pub cache: CacheConfig,
    /// Per-`UnscannableReason` policy.
    #[serde(default)]
    pub unscannable: UnscannablePolicy,
    /// Appservice scanning bypass.
    #[serde(default)]
    pub appservice_bypass: AppserviceBypass,
    /// `icap` provider settings. Required (and validated) when `provider == Icap`.
    #[serde(default)]
    pub icap: Option<IcapConfig>,
    /// `http` provider settings. Required when `provider == Http`.
    #[serde(default)]
    pub http: Option<HttpConfig>,
}

fn default_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_max_size() -> ByteSize {
    ByteSize::mib(100)
}

fn default_oversize() -> Action {
    Action::Quarantine
}

impl Default for ScanningConfig {
    fn default() -> Self {
        Self {
            mode: ScanMode::default(),
            provider: ProviderKind::default(),
            fail: None,
            allow_replacement: false,
            timeout: default_timeout(),
            max_size: default_max_size(),
            oversize: default_oversize(),
            cache: CacheConfig::default(),
            unscannable: UnscannablePolicy::default(),
            appservice_bypass: AppserviceBypass::default(),
            icap: None,
            http: None,
        }
    }
}

impl Validate for ScanningConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if self.mode != ScanMode::Off && self.fail.is_none() {
            errors.push(
                format!("{prefix}.fail"),
                "scanning is enabled (mode is not `off`) but no failure policy was chosen; set \
                 `fail: closed` or `fail: open` explicitly \u{2014} this is a policy decision, \
                 not a technical one, and is never defaulted silently",
            );
        }
        if self.mode != ScanMode::Off && self.provider == ProviderKind::None {
            errors.push(
                format!("{prefix}.provider"),
                "scanning is enabled (mode is not `off`) but provider is `none`; choose a \
                 provider or set mode to `off`",
            );
        }
        match self.provider {
            ProviderKind::Icap if self.icap.is_none() => {
                errors.push(
                    format!("{prefix}.icap"),
                    "provider is `icap` but no `icap` settings were given",
                );
            }
            ProviderKind::Http if self.http.is_none() => {
                errors.push(
                    format!("{prefix}.http"),
                    "provider is `http` but no `http` settings were given",
                );
            }
            _ => {}
        }
        if self.timeout.is_zero() {
            errors.push(format!("{prefix}.timeout"), "must be greater than 0");
        }
        if self.max_size.as_u64() == 0 {
            errors.push(format!("{prefix}.max_size"), "must be greater than 0");
        }
        if self.cache.capacity == 0 {
            errors.push(format!("{prefix}.cache.capacity"), "must be greater than 0");
        }
    }
}

impl ScanningConfig {
    /// Validates this config, returning [`MediaError::InvalidInput`] describing every problem
    /// found (not just the first) if it is not usable. This is the "configuration error" RFC
    /// section 5 requires for enabling scanning without a chosen failure policy.
    ///
    /// # Errors
    /// [`MediaError::InvalidInput`] listing every validation failure.
    pub fn validated(self) -> Result<Self, MediaError> {
        let mut errors = ValidationErrors::new();
        self.validate("media.scanning", &mut errors);
        if errors.is_empty() {
            Ok(self)
        } else {
            Err(MediaError::InvalidInput(format!(
                "invalid media.scanning configuration:\n{}",
                errors
                    .0
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            )))
        }
    }

    /// Parses a `media.scanning` YAML section on its own (for standalone testing and for
    /// whichever crate assembles the real config document to call once track 13 wires this
    /// section into `hs_config::MediaConfig` — see this module's doc comment).
    ///
    /// # Errors
    /// Returns a parse error wrapped as [`MediaError::InvalidInput`].
    pub fn from_yaml(yaml: &str) -> Result<Self, MediaError> {
        serde_yaml_ng::from_str(yaml)
            .map_err(|e| MediaError::InvalidInput(format!("invalid media.scanning YAML: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off_and_valid() {
        let cfg = ScanningConfig::default();
        assert_eq!(cfg.mode, ScanMode::Off);
        cfg.validated().unwrap();
    }

    #[test]
    fn enabling_scanning_without_a_fail_policy_is_a_config_error() {
        let cfg = ScanningConfig {
            mode: ScanMode::Block,
            provider: ProviderKind::Icap,
            icap: Some(IcapConfig {
                host: "c-icap".into(),
                port: 1344,
                service: "virus_scan".into(),
                preview: PreviewMode::Negotiate,
            }),
            fail: None,
            ..ScanningConfig::default()
        };
        let err = cfg.validated().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("fail"),
            "expected a fail-policy error, got: {msg}"
        );
    }

    #[test]
    fn enabling_scanning_with_a_fail_policy_and_provider_is_valid() {
        let cfg = ScanningConfig {
            mode: ScanMode::Block,
            provider: ProviderKind::Icap,
            icap: Some(IcapConfig {
                host: "c-icap".into(),
                port: 1344,
                service: "virus_scan".into(),
                preview: PreviewMode::Negotiate,
            }),
            fail: Some(FailPolicy::Closed),
            ..ScanningConfig::default()
        };
        cfg.validated().unwrap();
    }

    #[test]
    fn enabling_scanning_with_provider_none_is_a_config_error() {
        let cfg = ScanningConfig {
            mode: ScanMode::Block,
            provider: ProviderKind::None,
            fail: Some(FailPolicy::Closed),
            ..ScanningConfig::default()
        };
        let err = cfg.validated().unwrap_err();
        assert!(err.to_string().contains("provider"));
    }

    #[test]
    fn provider_without_its_settings_is_a_config_error() {
        let cfg = ScanningConfig {
            mode: ScanMode::Block,
            provider: ProviderKind::Http,
            http: None,
            fail: Some(FailPolicy::Open),
            ..ScanningConfig::default()
        };
        let err = cfg.validated().unwrap_err();
        assert!(err.to_string().contains("http"));
    }

    #[test]
    fn parses_the_rfcs_example_yaml() {
        let yaml = r#"
mode: block
provider: icap
allow_replacement: false
fail: closed
timeout: 30s
max_size: 100MiB
oversize: quarantine
cache:
  ttl: 7d
  unversioned_ttl: 1h
  capacity: 100000
icap:
  host: c-icap
  port: 1344
  service: virus_scan
  preview: negotiate
"#;
        let cfg = ScanningConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.mode, ScanMode::Block);
        assert_eq!(cfg.provider, ProviderKind::Icap);
        assert_eq!(cfg.fail, Some(FailPolicy::Closed));
        assert_eq!(cfg.timeout, Duration::from_secs(30));
        assert_eq!(cfg.max_size, ByteSize::mib(100));
        assert!(!cfg.allow_replacement);
        cfg.validated().unwrap();
    }

    #[test]
    fn off_mode_needs_no_fail_policy_even_with_a_provider_configured() {
        // An operator who configured a provider once and then dialed mode back to `off` (rather
        // than deleting the provider block) must not be blocked by validation.
        let cfg = ScanningConfig {
            mode: ScanMode::Off,
            provider: ProviderKind::Icap,
            icap: Some(IcapConfig {
                host: "c-icap".into(),
                port: 1344,
                service: "virus_scan".into(),
                preview: PreviewMode::Negotiate,
            }),
            fail: None,
            ..ScanningConfig::default()
        };
        cfg.validated().unwrap();
    }

    #[test]
    fn replacement_is_off_by_default() {
        assert!(!ScanningConfig::default().allow_replacement);
    }
}
