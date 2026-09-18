//! Configuration for pluggable content scanning (`docs/rfcs/0008-content-scanning.md`).
//!
//! This type deliberately does **not** live in `hs_config::MediaConfig` (track 13's crate, which
//! this track may not edit — see `docs/status/09-media.md`'s ownership rule). Instead it is a
//! self-contained, independently deserializable section (`media.scanning` in the operator's YAML)
//! that whoever assembles the real `hs_config::Config` document merges in — see this module's doc
//! on `ScanningConfig::from_media_section` for exactly how. This mirrors how RFC 0006 (URL
//! previews) already left new `MediaConfig` fields as a proposal rather than an edit.
//!
//! Reuses `hs_config::{Duration, ByteSize}` for parse compatibility with the rest of the config
//! system (`"30s"`, `"100MiB"`, ...) and `hs_config::Validate` for the same
//! `path -> message` validation-error shape every other config section uses.

use hs_config::error::{Validate, ValidationErrors};
use hs_config::{ByteSize, Duration};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::MediaError;

/// When scanning happens relative to the upload becoming servable (RFC section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    Off,
}

impl Default for ScanMode {
    fn default() -> Self {
        ScanMode::Off
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// No scanning (the default). `mode` is ignored if set to anything but `off` — see
    /// [`ScanningConfig::validate`].
    None,
    /// clamd `INSTREAM` over TCP or a Unix socket.
    ClamAv,
    /// The JSON submit-and-poll HTTP contract, e.g. CrowdStrike Falcon or any cloud/local HTTP
    /// scanner speaking this crate's protocol (`crate::scanning::providers::http`).
    Http,
    /// ICAP RESPMOD (RFC 3507): Symantec, McAfee, Sophos, Trend Micro, most vendor appliances.
    Icap,
    /// Spawn a local binary and feed it content on stdin.
    Command,
}

impl Default for ProviderKind {
    fn default() -> Self {
        ProviderKind::None
    }
}

/// How the `clamav` provider reaches clamd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum ClamAvAddress {
    /// `host:port` over TCP.
    Tcp {
        /// clamd's host.
        host: String,
        /// clamd's port.
        port: u16,
    },
    /// A Unix domain socket path (clamd's `LocalSocket`).
    Unix {
        /// Path to the socket, e.g. `/var/run/clamav/clamd.ctl`.
        path: String,
    },
}

impl Default for ClamAvAddress {
    fn default() -> Self {
        ClamAvAddress::Unix {
            path: "/var/run/clamav/clamd.ctl".to_string(),
        }
    }
}

/// `clamav` provider settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClamAvConfig {
    /// How to reach clamd.
    #[serde(default)]
    pub address: ClamAvAddress,
    /// Bytes per `INSTREAM` chunk (clamd's `StreamMaxLength` interacts with this; the default,
    /// 64 KiB, is comfortably under clamd's own default chunk ceiling).
    #[serde(default = "default_clamav_chunk_size")]
    pub chunk_size: usize,
}

fn default_clamav_chunk_size() -> usize {
    64 * 1024
}

/// `http` provider settings: the submit endpoint, and an optional separate poll endpoint for
/// providers that answer asynchronously (RFC section 3, "CrowdStrike is reachable ... through the
/// HTTP provider against the Falcon API, which is submit-then-poll").
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

/// `icap` provider settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IcapConfig {
    /// ICAP server host.
    pub host: String,
    /// ICAP server port (default 1344).
    #[serde(default = "default_icap_port")]
    pub port: u16,
    /// The ICAP service name, e.g. `avscan` (used in the request line
    /// `icap://<host>:<port>/<service>`).
    pub service: String,
}

fn default_icap_port() -> u16 {
    1344
}

/// `command` provider settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandConfig {
    /// The binary to spawn, e.g. `/usr/bin/clamscan`.
    pub path: String,
    /// Extra arguments, before the implicit `-` (read from stdin) most scanners expect.
    #[serde(default)]
    pub args: Vec<String>,
    /// Exit codes meaning "clean". Defaults to `[0]` (clamscan's convention).
    #[serde(default = "default_clean_codes")]
    pub clean_exit_codes: Vec<i32>,
    /// Exit codes meaning "infected". Defaults to `[1]` (clamscan's convention).
    #[serde(default = "default_infected_codes")]
    pub infected_exit_codes: Vec<i32>,
}

fn default_clean_codes() -> Vec<i32> {
    vec![0]
}

fn default_infected_codes() -> Vec<i32> {
    vec![1]
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

/// Top-level scanning configuration (`media.scanning` in the operator's YAML).
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
    /// `clamav` provider settings. Required (and validated) when `provider == ClamAv`.
    #[serde(default)]
    pub clamav: Option<ClamAvConfig>,
    /// `http` provider settings. Required when `provider == Http`.
    #[serde(default)]
    pub http: Option<HttpConfig>,
    /// `icap` provider settings. Required when `provider == Icap`.
    #[serde(default)]
    pub icap: Option<IcapConfig>,
    /// `command` provider settings. Required when `provider == Command`.
    #[serde(default)]
    pub command: Option<CommandConfig>,
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
            timeout: default_timeout(),
            max_size: default_max_size(),
            oversize: default_oversize(),
            cache: CacheConfig::default(),
            unscannable: UnscannablePolicy::default(),
            appservice_bypass: AppserviceBypass::default(),
            clamav: None,
            http: None,
            icap: None,
            command: None,
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
            ProviderKind::ClamAv if self.clamav.is_none() => {
                errors.push(format!("{prefix}.clamav"), "provider is `clamav` but no `clamav` settings were given");
            }
            ProviderKind::Http if self.http.is_none() => {
                errors.push(format!("{prefix}.http"), "provider is `http` but no `http` settings were given");
            }
            ProviderKind::Icap if self.icap.is_none() => {
                errors.push(format!("{prefix}.icap"), "provider is `icap` but no `icap` settings were given");
            }
            ProviderKind::Command if self.command.is_none() => {
                errors.push(format!("{prefix}.command"), "provider is `command` but no `command` settings were given");
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
            provider: ProviderKind::ClamAv,
            clamav: Some(ClamAvConfig::default()),
            fail: None,
            ..ScanningConfig::default()
        };
        let err = cfg.validated().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("fail"), "expected a fail-policy error, got: {msg}");
    }

    #[test]
    fn enabling_scanning_with_a_fail_policy_and_provider_is_valid() {
        let cfg = ScanningConfig {
            mode: ScanMode::Block,
            provider: ProviderKind::ClamAv,
            clamav: Some(ClamAvConfig::default()),
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
provider: clamav
fail: closed
timeout: 30s
max_size: 100MiB
oversize: quarantine
cache:
  ttl: 7d
  unversioned_ttl: 1h
  capacity: 100000
clamav:
  address:
    transport: unix
    path: /var/run/clamav/clamd.ctl
"#;
        let cfg = ScanningConfig::from_yaml(yaml).unwrap();
        assert_eq!(cfg.mode, ScanMode::Block);
        assert_eq!(cfg.provider, ProviderKind::ClamAv);
        assert_eq!(cfg.fail, Some(FailPolicy::Closed));
        assert_eq!(cfg.timeout, Duration::from_secs(30));
        assert_eq!(cfg.max_size, ByteSize::mib(100));
        cfg.validated().unwrap();
    }

    #[test]
    fn off_mode_needs_no_fail_policy_even_with_a_provider_configured() {
        // An operator who configured a provider once and then dialed mode back to `off` (rather
        // than deleting the provider block) must not be blocked by validation.
        let cfg = ScanningConfig {
            mode: ScanMode::Off,
            provider: ProviderKind::ClamAv,
            clamav: Some(ClamAvConfig::default()),
            fail: None,
            ..ScanningConfig::default()
        };
        cfg.validated().unwrap();
    }
}
