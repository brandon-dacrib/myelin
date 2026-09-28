//! Configuration for pluggable content adaptation (`docs/rfcs/0008-content-scanning.md`).
//!
//! The types live in `hs_config::scanning` and are the `media.scanning` section of the server's
//! configuration: stored in the database and edited through the admin API and the web interface
//! like every other setting (decision 0010), rather than read from a separate YAML file named on
//! the command line. They are re-exported here unchanged, so this crate's code names them where
//! it always has.
//!
//! [`ScanningConfig::validated`] returns `hs_config`'s own error; [`validated`] is the same check
//! returning this crate's [`MediaError`], which is what [`crate::scanning::ScanEngine::new`]
//! needs.

pub use hs_config::scanning::{
    Action, AppserviceBypass, CacheConfig, FailPolicy, HttpConfig, IcapConfig, PreviewMode,
    ProviderKind, ScanMode, ScanningConfig, UnscannablePolicy,
};

use crate::error::MediaError;

/// [`ScanningConfig::validated`], with the error as this crate's [`MediaError::InvalidInput`]
/// listing every problem found.
///
/// # Errors
/// [`MediaError::InvalidInput`] when the configuration is not usable.
pub fn validated(config: ScanningConfig) -> Result<ScanningConfig, MediaError> {
    config
        .validated()
        .map_err(|e| MediaError::InvalidInput(format!("invalid media.scanning configuration: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabling_scanning_without_a_fail_policy_is_an_invalid_input() {
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
        match validated(cfg) {
            Err(MediaError::InvalidInput(msg)) => assert!(msg.contains("fail"), "{msg}"),
            other => panic!("expected an invalid-input error, got {other:?}"),
        }
    }
}
