//! Provider adapters (`docs/rfcs/0008-content-scanning.md`, section 3).
//!
//! Only three: [`icap`] (the provider — everything reaches us through it), [`http`] (only for
//! cloud APIs with no ICAP fronting, notably CrowdStrike Falcon's submit-then-poll API), and
//! [`none`] (the default). A direct clamd client and a spawn-a-binary command runner were both
//! considered and deliberately **not** built, per
//! `docs/decisions/0007-build-less-reuse-more.md`: c-icap's `virus_scan` service already drives
//! ClamAV with packaged container images, so a second path to the same engine would be duplicated
//! maintenance forever rather than a saving. See `docs/status/09-media.md`'s "Reuse considered"
//! for the fuller record, including that this was a mid-session scope cut (the crate briefly had
//! `clamav`/`command` config scaffolding, which was removed before any client code existed for
//! either).
//!
//! [`build`] turns a [`crate::scanning::config::ScanningConfig`] into a boxed
//! [`crate::scanning::types::ContentScanner`], the one place that knows which provider module a
//! given [`crate::scanning::config::ProviderKind`] maps to.

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
                MediaError::InvalidInput(
                    "provider is `icap` but no `icap` settings were given".into(),
                )
            })?;
            Ok(Arc::new(icap::IcapScanner::new(icap_config.clone())))
        }
        ProviderKind::Http => {
            let http_config = config.http.as_ref().ok_or_else(|| {
                MediaError::InvalidInput(
                    "provider is `http` but no `http` settings were given".into(),
                )
            })?;
            Ok(Arc::new(http::HttpScanner::new(http_config.clone())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanning::config::{FailPolicy, IcapConfig, PreviewMode, ScanMode};

    #[test]
    fn builds_none_by_default() {
        let scanner = build(&ScanningConfig::default()).unwrap();
        assert_eq!(scanner.id(), "none");
    }

    #[test]
    fn builds_icap_when_configured() {
        let cfg = ScanningConfig {
            mode: ScanMode::Block,
            provider: ProviderKind::Icap,
            fail: Some(FailPolicy::Closed),
            icap: Some(IcapConfig {
                host: "c-icap".into(),
                port: 1344,
                service: "virus_scan".into(),
                preview: PreviewMode::Negotiate,
            }),
            ..ScanningConfig::default()
        };
        let scanner = build(&cfg).unwrap();
        assert_eq!(scanner.id(), "icap");
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
