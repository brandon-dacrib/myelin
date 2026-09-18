//! Day-one auth configuration.
//!
//! This struct predates `hs-config` (track 13's native config schema) and was originally this
//! crate's own stand-in, built ahead of that interface being frozen (`docs/workstreams/README.md`
//! rule 1: no dependency on another track's not-yet-frozen interface — see `docs/status/
//! 07-auth-and-identity.md`'s session 1/2 history). `hs-config` is now built and frozen, and
//! [`AuthConfig`]'s [`TryFrom<&hs_config::Config>`] impl below is the real bridge: every field
//! that has a direct `hs_config::AuthConfig` counterpart is mapped there, and every field that
//! does not is documented in that impl's doc comment rather than silently defaulted without
//! explanation. Every field here still names its Synapse config-option analog for the fields
//! `hs-config` doesn't cover yet.

use std::collections::HashSet;

use crate::error::MatrixError;

/// Configuration this crate needs to run the legacy auth surface.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// This homeserver's server name, used to build full user IDs from localparts. Stored
    /// pre-validated so handlers never need to handle a parse failure on every request.
    pub server_name: ruma::OwnedServerName,

    /// Static secret appended to every password before bcrypt verification of an imported hash.
    /// Synapse's `password_config.pepper`. Empty string if the imported deployment never set one.
    pub bcrypt_pepper: String,

    /// Whether `?access_token=` is still accepted alongside the `Authorization: Bearer` header.
    /// Synapse's default is to accept it; operators are expected to disable it eventually. Mixing
    /// both on the same request is always rejected regardless of this flag (that is not a
    /// leniency setting, it is ambiguous input).
    pub accept_legacy_query_param_token: bool,

    /// Lifetime of an access token from a non-refreshable login (`POST /login` without
    /// `refresh_token: true`), milliseconds. `None` means it never expires on its own (Synapse's
    /// default: `nonrefreshable_access_token_lifetime` unset).
    pub nonrefreshable_access_token_ttl_ms: Option<u64>,

    /// Lifetime of an access token minted as part of a refreshable session, milliseconds.
    /// Synapse's `refreshable_access_token_lifetime`, default 5 minutes.
    pub refreshable_access_token_ttl_ms: u64,

    /// Lifetime of a single refresh token, milliseconds. `None` means refresh tokens do not
    /// expire on their own (they are still single-use; rotation invalidates the previous one).
    /// Synapse's `refresh_token_lifetime`.
    pub refresh_token_ttl_ms: Option<u64>,

    /// Absolute cap on how long a refresh chain may extend a session, milliseconds, from the
    /// moment of login. `None` is unbounded. Synapse's `session_lifetime`.
    pub session_lifetime_ms: Option<u64>,

    /// Lifetime of a short-term login token (`m.login.token`), milliseconds. Synapse's default is
    /// two minutes.
    pub login_token_ttl_ms: u64,

    /// How long a UI-auth session stays valid between rounds, milliseconds. Synapse's
    /// `ui_auth_session_timeout`, default fifteen minutes... wait, this crate: enforced from the
    /// session's creation time on every round, not just the last one, matching
    /// [`crate::store::UiaStore::session_exists`]'s contract.
    pub uia_session_timeout_ms: u64,

    /// Whether `POST /register` accepts new accounts at all. Synapse's `enable_registration`.
    pub registration_enabled: bool,

    /// Whether registration requires a valid `m.login.registration_token` stage. Synapse's
    /// `registration_requires_token`.
    pub registration_requires_token: bool,

    /// The set of currently valid registration tokens. A day-one simplification of Synapse's
    /// token table (which also tracks per-token usage limits and expiry): every token here is
    /// valid for unlimited uses until removed. Extending this to per-token limits is a store
    /// change, not an API change, and is noted as future work in
    /// `docs/rfcs/0002-auth-tokens-and-requester.md`.
    pub valid_registration_tokens: HashSet<String>,

    /// Whether `POST /register?kind=guest` is accepted. Synapse's `allow_guest_access`.
    pub guest_registration_enabled: bool,

    /// Whether `m.login.recaptcha` is included in the registration UIA flows at all. When
    /// `false` (the default), the stage is never offered, so clients never hit it; when `true`,
    /// [`crate::routes::register`] fails the stage cleanly with `M_UNRECOGNIZED` because this
    /// server does not integrate with Google's verification service yet — see
    /// `docs/rfcs/0002-auth-tokens-and-requester.md` section 7 for the follow-up.
    pub recaptcha_enabled: bool,

    /// Whether `m.login.terms` is included in the registration flow. Synapse's
    /// `user_consent.require_at_registration`.
    pub terms_enabled: bool,

    /// The password policy enforced at registration and password change.
    pub password_policy: PasswordPolicy,

    /// The shared secret for the `com.devture.shared_secret_auth` login provider (legacy mautrix
    /// bridge double puppeting; `crate::shared_secret_auth`), if enabled. `None` (the default)
    /// means the login type is not offered and any attempt at it fails with `M_UNRECOGNIZED`,
    /// the same as any other unsupported login type. No dedicated `hs_config::AuthConfig` field
    /// exists for this yet (see `crate::shared_secret_auth`'s module doc and this crate's status
    /// file "Decisions made"); until track 13 adds one, `hs-config`'s
    /// `auth.registration_shared_secret` is reused for this purpose when wiring a native
    /// [`AuthConfig`] from it, on the reasoning that both are "a privileged shared secret an
    /// operator configures for trusted server-to-server tooling" and Synapse operators running
    /// the reference `devture` module already had to provision a *second* secret anyway — reusing
    /// the registration one here is strictly less new configuration surface, not more.
    pub shared_secret_auth_secret: Option<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            // "example.org" is a compile-time-known-valid server name; the `expect` cannot fail.
            server_name: ruma::ServerName::parse("example.org")
                .expect("\"example.org\" is a valid server name"),
            bcrypt_pepper: String::new(),
            accept_legacy_query_param_token: true,
            nonrefreshable_access_token_ttl_ms: None,
            refreshable_access_token_ttl_ms: 5 * 60 * 1000,
            refresh_token_ttl_ms: None,
            session_lifetime_ms: None,
            login_token_ttl_ms: 2 * 60 * 1000,
            uia_session_timeout_ms: 15 * 60 * 1000,
            registration_enabled: true,
            registration_requires_token: false,
            valid_registration_tokens: HashSet::new(),
            guest_registration_enabled: false,
            recaptcha_enabled: false,
            terms_enabled: false,
            password_policy: PasswordPolicy::default(),
            shared_secret_auth_secret: None,
        }
    }
}

/// An error converting a native [`hs_config::Config`] into this crate's [`AuthConfig`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigConversionError {
    /// `server.server_name` is not a syntactically valid Matrix server name. `hs-config`'s own
    /// validation only checks for emptiness/whitespace/length
    /// (`hs_config::server::ServerConfig::validate`), not full server-name grammar, so this is
    /// checked again here where a `ruma::ServerName` is actually required.
    #[error("server.server_name {0:?} is not a valid Matrix server name: {1}")]
    InvalidServerName(String, ruma::IdParseError),
}

impl TryFrom<&hs_config::Config> for AuthConfig {
    type Error = ConfigConversionError;

    /// Builds this crate's [`AuthConfig`] from a native [`hs_config::Config`]. Only the fields
    /// that have a direct `hs_config::AuthConfig` counterpart are mapped; the rest keep
    /// [`AuthConfig::default`]'s values, documented below rather than silently inherited:
    ///
    /// - `nonrefreshable_access_token_ttl_ms`, `session_lifetime_ms`, `login_token_ttl_ms`,
    ///   `uia_session_timeout_ms`: no `hs_config::AuthConfig` field exists yet for any of these
    ///   (day-one gaps on the `hs-config` side, not this conversion's job to invent — an RFC to
    ///   `hs-config` is the right next step if an operator needs to tune one of them).
    /// - `registration_requires_token`, `valid_registration_tokens`: `hs-config` has no
    ///   registration-token config surface yet (this crate's own `valid_registration_tokens` is
    ///   day-one, in-process-only storage besides — see that field's own doc comment).
    /// - `guest_registration_enabled`, `recaptcha_enabled`, `terms_enabled`,
    ///   `accept_legacy_query_param_token`: same — no native config field yet.
    ///
    /// `shared_secret_auth_secret` (the `com.devture.shared_secret_auth` login provider) has no
    /// dedicated `hs_config` field either, but is deliberately **not** left at its default:
    /// `hs_config::AuthConfig::registration_shared_secret` is reused for it (see
    /// `docs/status/07-auth-and-identity.md`'s "Decisions made" for the full reasoning — both are
    /// "a privileged shared secret for trusted server-to-server tooling", and reusing one avoids
    /// new operator-facing config surface for a legacy bridge compatibility feature).
    ///
    /// # Errors
    /// Returns [`ConfigConversionError::InvalidServerName`] if `config.server.server_name` does
    /// not parse as a [`ruma::ServerName`].
    fn try_from(config: &hs_config::Config) -> Result<Self, Self::Error> {
        let server_name: ruma::OwnedServerName =
            ruma::ServerName::parse(&config.server.server_name).map_err(|e| {
                ConfigConversionError::InvalidServerName(config.server.server_name.clone(), e)
            })?;

        let mut auth = Self {
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
            // See this method's doc comment: deliberately reused, not left at the default.
            shared_secret_auth_secret: config
                .auth
                .registration_shared_secret
                .as_str()
                .map(str::to_owned),
            ..Self::default()
        };

        // `hs_config`'s password policy always carries a `minimum_length` (defaulting to 8, never
        // absent); this crate's wants `Option<usize>` (its own default is `None`, "no minimum").
        // A native config's minimum is therefore always applied, matching what an operator who
        // wrote (or accepted the default of) `auth.password.policy.minimum_length: 8` would
        // expect.
        auth.password_policy = PasswordPolicy {
            minimum_length: Some(config.auth.password.policy.minimum_length as usize),
            require_digit: config.auth.password.policy.require_digit,
            require_symbol: config.auth.password.policy.require_symbol,
            require_lowercase: config.auth.password.policy.require_lowercase,
            require_uppercase: config.auth.password.policy.require_uppercase,
        };

        Ok(auth)
    }
}

/// A password strength policy, matching the spec's `GET /password_policy` field names
/// (`m.minimum_length`, `m.require_digit`, ...).
#[derive(Debug, Clone, Default)]
pub struct PasswordPolicy {
    /// Minimum length in Unicode scalar values, if any.
    pub minimum_length: Option<usize>,
    /// Require at least one ASCII digit.
    pub require_digit: bool,
    /// Require at least one non-alphanumeric character.
    pub require_symbol: bool,
    /// Require at least one lowercase letter.
    pub require_lowercase: bool,
    /// Require at least one uppercase letter.
    pub require_uppercase: bool,
}

impl PasswordPolicy {
    /// Checks `password` against the policy, returning `400 M_WEAK_PASSWORD` naming the first
    /// unmet rule.
    ///
    /// # Errors
    /// Returns [`MatrixError::weak_password`] if any configured rule is not met.
    pub fn validate(&self, password: &str) -> Result<(), MatrixError> {
        if let Some(min) = self.minimum_length
            && password.chars().count() < min
        {
            return Err(MatrixError::weak_password(format!(
                "Password too short (minimum {min} characters)"
            )));
        }
        if self.require_digit && !password.chars().any(|c| c.is_ascii_digit()) {
            return Err(MatrixError::weak_password("Password must contain a digit"));
        }
        if self.require_lowercase && !password.chars().any(|c| c.is_ascii_lowercase()) {
            return Err(MatrixError::weak_password(
                "Password must contain a lowercase letter",
            ));
        }
        if self.require_uppercase && !password.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(MatrixError::weak_password(
                "Password must contain an uppercase letter",
            ));
        }
        if self.require_symbol && !password.chars().any(|c| !c.is_alphanumeric()) {
            return Err(MatrixError::weak_password("Password must contain a symbol"));
        }
        Ok(())
    }

    /// The `GET /password_policy` response body.
    #[must_use]
    pub fn to_response_json(&self) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        if let Some(min) = self.minimum_length {
            body.insert("m.minimum_length".to_string(), min.into());
        }
        body.insert("m.require_digit".to_string(), self.require_digit.into());
        body.insert("m.require_symbol".to_string(), self.require_symbol.into());
        body.insert(
            "m.require_lowercase".to_string(),
            self.require_lowercase.into(),
        );
        body.insert(
            "m.require_uppercase".to_string(),
            self.require_uppercase.into(),
        );
        serde_json::Value::Object(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_accepts_anything() {
        let policy = PasswordPolicy::default();
        assert!(policy.validate("a").is_ok());
    }

    #[test]
    fn minimum_length_is_enforced() {
        let policy = PasswordPolicy {
            minimum_length: Some(8),
            ..Default::default()
        };
        assert!(policy.validate("short").is_err());
        assert!(policy.validate("longenough").is_ok());
    }

    #[test]
    fn each_character_class_rule_is_enforced_independently() {
        let policy = PasswordPolicy {
            require_digit: true,
            require_symbol: true,
            require_lowercase: true,
            require_uppercase: true,
            ..Default::default()
        };
        assert!(policy.validate("Ab1!").is_ok());
        assert!(policy.validate("ab1!").is_err()); // no uppercase
        assert!(policy.validate("AB1!").is_err()); // no lowercase
        assert!(policy.validate("Abc!").is_err()); // no digit
        assert!(policy.validate("Ab12").is_err()); // no symbol
    }

    #[test]
    fn response_json_has_spec_field_names() {
        let policy = PasswordPolicy {
            minimum_length: Some(8),
            require_digit: true,
            ..Default::default()
        };
        let json = policy.to_response_json();
        assert_eq!(json["m.minimum_length"], 8);
        assert_eq!(json["m.require_digit"], true);
        assert_eq!(json["m.require_symbol"], false);
    }

    fn minimal_native_config() -> hs_config::Config {
        hs_config::Config::from_yaml("server:\n  server_name: example.org\n").unwrap()
    }

    #[test]
    fn try_from_maps_server_name_and_registration_flag() {
        let mut config = minimal_native_config();
        config.auth.enable_registration = true;
        let auth = AuthConfig::try_from(&config).unwrap();
        assert_eq!(auth.server_name.as_str(), "example.org");
        assert!(auth.registration_enabled);
    }

    #[test]
    fn try_from_rejects_an_invalid_server_name() {
        // `hs-config` itself rejects whitespace in `server_name` at load time, so this never
        // reaches the conversion in practice via YAML; this test documents that the conversion's
        // own check is defense in depth, not dead code, by constructing a `Config` value directly.
        let mut config = minimal_native_config();
        config.server.server_name = "not a valid name".to_owned();
        assert!(AuthConfig::try_from(&config).is_err());
    }

    #[test]
    fn try_from_password_policy_minimum_length_is_always_some() {
        let config = minimal_native_config();
        let auth = AuthConfig::try_from(&config).unwrap();
        assert_eq!(auth.password_policy.minimum_length, Some(8));
    }

    #[test]
    fn try_from_maps_token_lifetimes() {
        let mut config = minimal_native_config();
        config.auth.access_token_lifetime = hs_config::Duration::from_hours(2);
        config.auth.refresh_token_lifetime = Some(hs_config::Duration::from_days(30));
        let auth = AuthConfig::try_from(&config).unwrap();
        assert_eq!(auth.refreshable_access_token_ttl_ms, 2 * 60 * 60 * 1000);
        assert_eq!(auth.refresh_token_ttl_ms, Some(30 * 24 * 60 * 60 * 1000));
    }

    #[test]
    fn try_from_reuses_registration_shared_secret_for_shared_secret_auth() {
        let mut config = minimal_native_config();
        assert!(
            AuthConfig::try_from(&config)
                .unwrap()
                .shared_secret_auth_secret
                .is_none(),
            "unset registration_shared_secret maps to the feature being disabled"
        );

        config.auth.registration_shared_secret = hs_config::SecretString::from("topsecret");
        let auth = AuthConfig::try_from(&config).unwrap();
        assert_eq!(auth.shared_secret_auth_secret.as_deref(), Some("topsecret"));
    }

    #[test]
    fn try_from_leaves_fields_with_no_native_counterpart_at_their_default() {
        let config = minimal_native_config();
        let auth = AuthConfig::try_from(&config).unwrap();
        let default = AuthConfig::default();
        assert_eq!(
            auth.nonrefreshable_access_token_ttl_ms,
            default.nonrefreshable_access_token_ttl_ms
        );
        assert_eq!(auth.session_lifetime_ms, default.session_lifetime_ms);
        assert_eq!(auth.login_token_ttl_ms, default.login_token_ttl_ms);
        assert_eq!(auth.uia_session_timeout_ms, default.uia_session_timeout_ms);
        assert_eq!(
            auth.registration_requires_token,
            default.registration_requires_token
        );
        assert_eq!(
            auth.guest_registration_enabled,
            default.guest_registration_enabled
        );
        assert_eq!(auth.recaptcha_enabled, default.recaptcha_enabled);
        assert_eq!(auth.terms_enabled, default.terms_enabled);
        assert_eq!(
            auth.accept_legacy_query_param_token,
            default.accept_legacy_query_param_token
        );
    }
}
