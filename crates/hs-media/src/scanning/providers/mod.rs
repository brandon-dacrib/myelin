//! Provider adapters (`docs/rfcs/0008-content-scanning.md`, section 3).
//!
//! Build order and relative depth follow the RFC's own priority, set by the integration lead
//! after this session had already started on a five-way-equal plan: **ICAP is the primary
//! adapter** (nearly every scanner an operator might want — ClamAV via `c-icap`, the commercial
//! engines natively, cloud APIs via an ICAP gateway — is reachable through it), so
//! [`icap`] is the deep, protocol-complete implementation (OPTIONS negotiation, preview mode,
//! `204`, `Transfer-Ignore`, connection reuse, `ISTag`-as-`engine_version`). [`clamav`] is kept as
//! a one-hop optimization for operators who would rather not also run `c-icap`, not a peer in
//! effort. [`http`] and [`command`] cover what ICAP cannot (CrowdStrike Falcon's submit-and-poll
//! API; a local binary). [`none`] is the default.
//!
//! [`build`] turns a [`crate::scanning::config::ScanningConfig`] into a boxed
//! [`crate::scanning::types::ContentScanner`], the one place that knows which provider module a
//! given [`crate::scanning::config::ProviderKind`] maps to.

pub mod clamav;
pub mod command;
pub mod http;
pub mod icap;
pub mod none;

use std::sync::Arc;

use crate::error::MediaError;
use crate::scanning::config::{ProviderKind, ScanningConfig};
use crate::scanning::types::ContentScanner;

/// Builds the configured provider. `config` should already have passed
/// [`crate::scanning::config::ScanningConfig::validated`] (this function re-derives the same
/// "provider needs its settings" checks defensively, but the friendlier, all-problems-at-once
/// error message is `validated`'s).
///
/// # Errors
/// [`MediaError::InvalidInput`] if the selected provider's settings are missing or malformed.
pub fn build(config: &ScanningConfig) -> Result<Arc<dyn ContentScanner>, MediaError> {
    match config.provider {
        ProviderKind::None => Ok(Arc::new(none::NoneScanner)),
        ProviderKind::Icap => {
            let icap_config = config.icap.as_ref().ok_or_else(|| {
                MediaError::InvalidInput("provider is `icap` but no `icap` settings were given".into())
            })?;
            Ok(Arc::new(icap::IcapScanner::new(icap_config.clone())))
        }
        ProviderKind::ClamAv => {
            let clamav_config = config.clamav.as_ref().ok_or_else(|| {
                MediaError::InvalidInput(
                    "provider is `clamav` but no `clamav` settings were given".into(),
                )
            })?;
            Ok(Arc::new(clamav::ClamAvScanner::new(clamav_config.clone())))
        }
        ProviderKind::Http => {
            let http_config = config.http.as_ref().ok_or_else(|| {
                MediaError::InvalidInput("provider is `http` but no `http` settings were given".into())
            })?;
            Ok(Arc::new(http::HttpScanner::new(http_config.clone())))
        }
        ProviderKind::Command => {
            let command_config = config.command.as_ref().ok_or_else(|| {
                MediaError::InvalidInput(
                    "provider is `command` but no `command` settings were given".into(),
                )
            })?;
            Ok(Arc::new(command::CommandScanner::new(command_config.clone())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanning::config::{ClamAvConfig, FailPolicy, ScanMode};

    #[test]
    fn builds_none_by_default() {
        let scanner = build(&ScanningConfig::default()).unwrap();
        assert_eq!(scanner.id(), "none");
    }

    #[test]
    fn builds_clamav_when_configured() {
        let cfg = ScanningConfig {
            mode: ScanMode::Block,
            provider: ProviderKind::ClamAv,
            fail: Some(FailPolicy::Closed),
            clamav: Some(ClamAvConfig::default()),
            ..ScanningConfig::default()
        };
        let scanner = build(&cfg).unwrap();
        assert_eq!(scanner.id(), "clamav");
    }

    #[test]
    fn missing_provider_settings_is_an_error() {
        let cfg = ScanningConfig {
            provider: ProviderKind::Icap,
            icap: None,
            ..ScanningConfig::default()
        };
        assert!(build(&cfg).is_err());
    }
}
