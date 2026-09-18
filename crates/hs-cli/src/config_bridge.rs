//! Bridges between `hs-config`'s native configuration schema and the config types the crates
//! `hs serve` mounts actually expect.
//!
//! This module exists because of a seam gap discovered while wiring `hs serve` together for the
//! first time (see `docs/status/12-platform-and-kubernetes.md`, "Decisions made" /
//! "Interfaces needed"): `hs-auth::config::AuthConfig` is **not** the same type as
//! `hs_config::AuthConfig`. `hs-auth`'s own doc comment on its config module explains why (track
//! 07 built its own stand-in ahead of `hs-config` landing, per `docs/workstreams/README.md` rule
//! 1: "no dependency on another track's not-yet-frozen interface"), and it has not yet been
//! rewired now that `hs-config` exists. `hs-cli` is not `hs-auth`'s owner and does not edit it;
//! this module is the mechanical field-by-field bridge until track 07 does that rewiring itself
//! (tracked as an interface-needed item in the status file, not fixed here).

use ruma::OwnedServerName;

/// Errors bridging a native [`hs_config::Config`] into the types other crates expect.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// `server.server_name` is not a syntactically valid Matrix server name. `hs-config`'s own
    /// validation only checks for emptiness/whitespace/length (see
    /// `hs_config::server::ServerConfig::validate`), not full server-name grammar, so this is
    /// checked again here where a `ruma::ServerName` is actually required.
    #[error("server.server_name {0:?} is not a valid Matrix server name: {1}")]
    InvalidServerName(String, ruma::IdParseError),
}

/// Builds the `hs-auth` crate's own (pre-`hs-config`) [`hs_auth::config::AuthConfig`] from a
/// native [`hs_config::Config`]. Only the fields that have a direct counterpart are mapped; the
/// rest keep `hs_auth::config::AuthConfig::default()`'s values (documented per-field below).
///
/// # Errors
/// Returns [`BridgeError::InvalidServerName`] if `config.server.server_name` does not parse as a
/// [`ruma::ServerName`].
pub fn auth_config_from(
    config: &hs_config::Config,
) -> Result<hs_auth::config::AuthConfig, BridgeError> {
    let server_name: OwnedServerName = ruma::ServerName::parse(&config.server.server_name)
        .map_err(|e| BridgeError::InvalidServerName(config.server.server_name.clone(), e))?;

    let mut auth = hs_auth::config::AuthConfig {
        server_name,
        bcrypt_pepper: config
            .auth
            .password
            .pepper
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        refreshable_access_token_ttl_ms: config.auth.access_token_lifetime.as_millis(),
        refresh_token_ttl_ms: config.auth.refresh_token_lifetime.map(|d| d.as_millis()),
        registration_enabled: config.auth.enable_registration,
        ..hs_auth::config::AuthConfig::default()
    };

    // `hs_config`'s password policy always carries a `minimum_length` (defaulting to 8, never
    // absent); `hs-auth`'s wants `Option<usize>` (its own default is `None`, "no minimum"). A
    // native config's minimum is therefore always applied, matching what an operator who wrote
    // (or accepted the default of) `auth.password.policy.minimum_length: 8` would expect.
    auth.password_policy = hs_auth::config::PasswordPolicy {
        minimum_length: Some(config.auth.password.policy.minimum_length as usize),
        require_digit: config.auth.password.policy.require_digit,
        require_symbol: config.auth.password.policy.require_symbol,
        require_lowercase: config.auth.password.policy.require_lowercase,
        require_uppercase: config.auth.password.policy.require_uppercase,
    };

    // The fields below have no native `hs-config` counterpart at all (day-one gaps on the
    // `hs-config` side, not this bridge's job to invent): `nonrefreshable_access_token_ttl_ms`,
    // `session_lifetime_ms`, `login_token_ttl_ms`, `uia_session_timeout_ms`,
    // `registration_requires_token`, `valid_registration_tokens`, `guest_registration_enabled`,
    // `recaptcha_enabled`, `terms_enabled`, `accept_legacy_query_param_token`. They keep
    // `hs_auth::config::AuthConfig::default()`'s values via the `..` spread above.

    Ok(auth)
}

/// Builds a [`hs_telemetry::init::Options`] from a native [`hs_config::Config`]'s
/// `telemetry` section, plus the service name/version this binary reports.
#[must_use]
pub fn telemetry_options_from(
    config: &hs_config::Config,
    service_name: &str,
    service_version: &str,
) -> hs_telemetry::Options {
    use hs_config::telemetry::LogLevel;
    let level = match config.telemetry.logging.level {
        LogLevel::Trace => hs_telemetry::Level::Trace,
        LogLevel::Debug => hs_telemetry::Level::Debug,
        LogLevel::Info => hs_telemetry::Level::Info,
        LogLevel::Warn => hs_telemetry::Level::Warn,
        LogLevel::Error => hs_telemetry::Level::Error,
    };
    let format = if config.telemetry.logging.json {
        hs_telemetry::LogFormat::Json
    } else {
        hs_telemetry::LogFormat::SynapseText
    };
    hs_telemetry::Options {
        level,
        format,
        otlp_endpoint: config.telemetry.tracing.enabled.then(|| {
            config
                .telemetry
                .tracing
                .otlp_endpoint
                .clone()
                .unwrap_or_default()
        }),
        otlp_sample_ratio: config.telemetry.tracing.sample_ratio,
        sentry_dsn: config
            .telemetry
            .sentry
            .as_ref()
            .and_then(|s| s.dsn.as_str().map(str::to_owned)),
        sentry_environment: config
            .telemetry
            .sentry
            .as_ref()
            .map(|s| s.environment.clone())
            .unwrap_or_else(|| "production".to_owned()),
        service_name: service_name.to_owned(),
        service_version: service_version.to_owned(),
    }
}

/// Detects whether a config file's contents look like a Synapse `homeserver.yaml` or a native
/// `hs-config` file, matching `docs/compat/cli-shims.md`'s "the same way `hs serve` would — a
/// `server_name` key present either way" rule for `hs register -c` / `hs hash-password -c`: the
/// discriminator is whether the top-level mapping has a `server` sub-mapping (native) or a
/// top-level `server_name` scalar (Synapse) — both formats key off `server_name` textually, so
/// the structural shape (nested vs. flat) is what actually distinguishes them.
#[must_use]
pub fn looks_like_native_config(contents: &str) -> bool {
    let Ok(value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(contents) else {
        return false;
    };
    let Some(mapping) = value.as_mapping() else {
        return false;
    };
    mapping
        .get(serde_yaml_ng::Value::String("server".to_owned()))
        .and_then(|v| v.as_mapping())
        .is_some()
}

/// Reads `auth.registration_shared_secret` from either a native or a Synapse config file at
/// `path`, per `hs register -c` / `hs hash-password -c`'s config-file detection.
///
/// # Errors
/// Returns an error string suitable for printing to stderr and exiting non-zero: the file could
/// not be read, was not valid YAML/native config, or had no shared secret configured.
pub fn read_shared_secret_from_config(path: &std::path::Path) -> Result<String, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read {path:?}: {e}"))?;
    let config = if looks_like_native_config(&contents) {
        hs_config::Config::from_yaml(&contents).map_err(|e| e.to_string())?
    } else {
        let (config, _report) = hs_compat::translate::translate(
            &contents,
            hs_compat::TranslateOptions {
                allow_unsupported: true,
            },
        )
        .map_err(|e| e.to_string())?;
        config
    };
    config
        .auth
        .registration_shared_secret
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{path:?} has no auth.registration_shared_secret configured"))
}

/// Reads `auth.password.pepper` the same way [`read_shared_secret_from_config`] reads the
/// shared secret, for `hs hash-password -c`.
///
/// # Errors
/// Same as [`read_shared_secret_from_config`], except an absent pepper is not an error (an empty
/// pepper is a normal, valid configuration).
pub fn read_pepper_from_config(path: &std::path::Path) -> Result<String, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read {path:?}: {e}"))?;
    let config = if looks_like_native_config(&contents) {
        hs_config::Config::from_yaml(&contents).map_err(|e| e.to_string())?
    } else {
        let (config, _report) = hs_compat::translate::translate(
            &contents,
            hs_compat::TranslateOptions {
                allow_unsupported: true,
            },
        )
        .map_err(|e| e.to_string())?;
        config
    };
    Ok(config
        .auth
        .password
        .pepper
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> hs_config::Config {
        hs_config::Config::from_yaml("server:\n  server_name: example.org\n").unwrap()
    }

    #[test]
    fn maps_server_name_and_registration_flag() {
        let mut config = minimal_config();
        config.auth.enable_registration = true;
        let auth = auth_config_from(&config).unwrap();
        assert_eq!(auth.server_name.as_str(), "example.org");
        assert!(auth.registration_enabled);
    }

    #[test]
    fn invalid_server_name_is_rejected() {
        let yaml = "server:\n  server_name: \"not a valid name\"\n";
        // hs-config itself rejects whitespace in server_name, so this never reaches the bridge in
        // practice; this test documents that the bridge's own check is defense in depth, not
        // dead code, by constructing a `Config` value directly rather than parsing YAML.
        let mut config = minimal_config();
        config.server.server_name = "not a valid name".to_owned();
        let _ = yaml;
        assert!(auth_config_from(&config).is_err());
    }

    #[test]
    fn password_policy_minimum_length_is_always_some() {
        let config = minimal_config();
        let auth = auth_config_from(&config).unwrap();
        assert_eq!(auth.password_policy.minimum_length, Some(8));
    }

    #[test]
    fn detects_native_vs_synapse_config_shape() {
        assert!(looks_like_native_config(
            "server:\n  server_name: example.org\n"
        ));
        assert!(!looks_like_native_config("server_name: example.org\n"));
    }
}
