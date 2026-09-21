//! Authentication and authorization: native OAuth 2.0 issuer settings,
//! legacy login, password policy, registration, upstream OIDC providers and
//! MAS delegation mode. Restart required to change (see [`crate::reload`]):
//! session-signing secrets are cached in every issued token.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::Duration;
use crate::error::{Validate, ValidationErrors};
use crate::secret::{SecretString, resolve_secret_pair};

const fn default_true() -> bool {
    true
}

fn default_access_token_lifetime() -> Duration {
    Duration::from_hours(1)
}

fn default_refresh_token_lifetime() -> Option<Duration> {
    Some(Duration::from_days(365))
}

fn default_minimum_password_length() -> u32 {
    8
}

/// Password login and policy. Corresponds to Synapse's `password_config`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PasswordConfig {
    /// Allow password login at all. Corresponds to Synapse's
    /// `password_config.enabled`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Inline pepper mixed into password hashes. Prefer `pepper_file`.
    /// Corresponds to Synapse's `password_config.pepper`.
    #[serde(default)]
    pub pepper: SecretString,
    /// Path to a file containing the pepper.
    #[serde(default)]
    pub pepper_file: Option<PathBuf>,
    /// Complexity requirements. Corresponds to Synapse's
    /// `password_config.policy`.
    #[serde(default)]
    pub policy: PasswordPolicy,
}

impl Default for PasswordConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pepper: SecretString::default(),
            pepper_file: None,
            policy: PasswordPolicy::default(),
        }
    }
}

/// Password complexity requirements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PasswordPolicy {
    /// Minimum length.
    #[serde(default = "default_minimum_password_length")]
    pub minimum_length: u32,
    /// Require at least one digit.
    #[serde(default)]
    pub require_digit: bool,
    /// Require at least one symbol.
    #[serde(default)]
    pub require_symbol: bool,
    /// Require at least one uppercase letter.
    #[serde(default)]
    pub require_uppercase: bool,
    /// Require at least one lowercase letter.
    #[serde(default)]
    pub require_lowercase: bool,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            minimum_length: default_minimum_password_length(),
            require_digit: false,
            require_symbol: false,
            require_uppercase: false,
            require_lowercase: false,
        }
    }
}

/// One upstream OIDC identity provider. Corresponds to one entry in
/// Synapse's `oidc_providers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OidcProviderConfig {
    /// Stable identifier used in the login flow and stored on the user's
    /// external identity. Corresponds to Synapse's `idp_id`.
    pub idp_id: String,
    /// Display name shown on the login page. Corresponds to Synapse's
    /// `idp_name`.
    #[serde(default)]
    pub idp_name: Option<String>,
    /// The provider's issuer URL (used for discovery).
    pub issuer: String,
    /// OAuth client ID registered with the provider.
    pub client_id: String,
    /// Inline client secret. Prefer `client_secret_file`.
    #[serde(default)]
    pub client_secret: SecretString,
    /// Path to a file containing the client secret.
    #[serde(default)]
    pub client_secret_file: Option<PathBuf>,
    /// OAuth scopes to request.
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
}

fn default_oidc_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into()]
}

/// Matrix Authentication Service delegation mode: this server introspects
/// tokens against MAS instead of running its own OAuth issuer. Corresponds
/// to Synapse's `experimental_features.msc3861` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MasDelegationConfig {
    /// MAS's internal endpoint for introspection and provisioning calls.
    pub endpoint: String,
    /// Inline shared secret authenticating this server to MAS. Prefer
    /// `shared_secret_file`.
    #[serde(default)]
    pub shared_secret: SecretString,
    /// Path to a file containing the shared secret.
    #[serde(default)]
    pub shared_secret_file: Option<PathBuf>,
}

/// Authentication, session and registration settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Allow `POST /register`. Corresponds to Synapse's
    /// `enable_registration`.
    #[serde(default)]
    pub enable_registration: bool,
    /// Inline shared secret for the `/_synapse/mk_admin_user`-equivalent
    /// shared-secret registration protocol (see `hs-compat`). Prefer
    /// `registration_shared_secret_file`. Corresponds to Synapse's
    /// `registration_shared_secret`.
    #[serde(default)]
    pub registration_shared_secret: SecretString,
    /// Path to a file containing the shared-secret-registration secret.
    #[serde(default)]
    pub registration_shared_secret_file: Option<PathBuf>,
    /// Let the user directory (`POST /user_directory/search`, the box a client's invite dialog
    /// searches) find every account on this server. Off by default: a search then finds only
    /// the people the searcher shares a room with and the members of public rooms, which is
    /// what the Matrix specification requires and no more. Turning it on lets people find
    /// somebody they have not met yet -- convenient on a small server where everyone knows
    /// everyone -- at the cost that any account can list every other account's name,
    /// including the accounts a bridge creates for other people's contacts. Corresponds to
    /// Synapse's `user_directory.search_all_users`.
    #[serde(default)]
    pub user_directory_search_all_users: bool,
    /// Serve the legacy `/login` and user-interactive-auth flows in
    /// addition to the native OAuth 2.0 issuer. Needed for older clients,
    /// bridges and `m.login.application_service`.
    #[serde(default = "default_true")]
    pub enable_legacy_login: bool,
    /// Inline key signing issued access/refresh tokens and session
    /// cookies. Prefer `session_secret_file`. Corresponds to Synapse's
    /// `macaroon_secret_key`.
    #[serde(default)]
    pub session_secret: SecretString,
    /// Path to a file containing the session-signing secret.
    #[serde(default)]
    pub session_secret_file: Option<PathBuf>,
    /// Access token lifetime. Corresponds to Synapse's
    /// `access_token_lifetime` (native OAuth tokens; legacy non-refreshable
    /// tokens are unaffected, matching Synapse's own carve-out).
    #[serde(default = "default_access_token_lifetime")]
    pub access_token_lifetime: Duration,
    /// Refresh token lifetime; `None` means refresh tokens do not expire.
    /// Corresponds to Synapse's `refreshable_access_token_lifetime`
    /// family.
    #[serde(default = "default_refresh_token_lifetime")]
    pub refresh_token_lifetime: Option<Duration>,
    /// Password login settings.
    #[serde(default)]
    pub password: PasswordConfig,
    /// Upstream OIDC providers.
    #[serde(default)]
    pub oidc_providers: Vec<OidcProviderConfig>,
    /// When set, delegate to Matrix Authentication Service instead of
    /// running the native OAuth issuer.
    #[serde(default)]
    pub mas_delegation: Option<MasDelegationConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enable_registration: false,
            user_directory_search_all_users: false,
            registration_shared_secret: SecretString::default(),
            registration_shared_secret_file: None,
            enable_legacy_login: true,
            session_secret: SecretString::default(),
            session_secret_file: None,
            access_token_lifetime: default_access_token_lifetime(),
            refresh_token_lifetime: default_refresh_token_lifetime(),
            password: PasswordConfig::default(),
            oidc_providers: Vec::new(),
            mas_delegation: None,
        }
    }
}

impl Validate for AuthConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if self.access_token_lifetime.is_zero() {
            errors.push(
                format!("{prefix}.access_token_lifetime"),
                "must be greater than 0",
            );
        }
        if self.password.policy.minimum_length == 0 {
            errors.push(
                format!("{prefix}.password.policy.minimum_length"),
                "must be at least 1",
            );
        }
        let mut seen_idp_ids = std::collections::HashSet::new();
        for (i, p) in self.oidc_providers.iter().enumerate() {
            let path = format!("{prefix}.oidc_providers[{i}]");
            if p.idp_id.trim().is_empty() {
                errors.push(format!("{path}.idp_id"), "must not be empty");
            } else if !seen_idp_ids.insert(p.idp_id.clone()) {
                errors.push(
                    format!("{path}.idp_id"),
                    format!("{:?} is used by more than one provider", p.idp_id),
                );
            }
            if p.issuer.trim().is_empty() {
                errors.push(format!("{path}.issuer"), "must not be empty");
            }
            if p.client_id.trim().is_empty() {
                errors.push(format!("{path}.client_id"), "must not be empty");
            }
        }
        if let Some(mas) = &self.mas_delegation
            && mas.endpoint.trim().is_empty()
        {
            errors.push(
                format!("{prefix}.mas_delegation.endpoint"),
                "must not be empty",
            );
        }
        if self.mas_delegation.is_some() && !self.oidc_providers.is_empty() {
            errors.push(
                format!("{prefix}.mas_delegation"),
                "cannot be combined with oidc_providers: MAS is itself the OIDC issuer in delegation mode",
            );
        }
    }
}

impl AuthConfig {
    /// Resolves every `*_file` secret this section carries.
    pub(crate) fn resolve_secrets(&mut self, prefix: &str) -> Result<(), ConfigError> {
        resolve_secret_pair(
            &format!("{prefix}.registration_shared_secret"),
            &mut self.registration_shared_secret,
            &self.registration_shared_secret_file,
        )?;
        resolve_secret_pair(
            &format!("{prefix}.session_secret"),
            &mut self.session_secret,
            &self.session_secret_file,
        )?;
        resolve_secret_pair(
            &format!("{prefix}.password.pepper"),
            &mut self.password.pepper,
            &self.password.pepper_file,
        )?;
        for (i, p) in self.oidc_providers.iter_mut().enumerate() {
            resolve_secret_pair(
                &format!("{prefix}.oidc_providers[{i}].client_secret"),
                &mut p.client_secret,
                &p.client_secret_file,
            )?;
        }
        if let Some(mas) = &mut self.mas_delegation {
            resolve_secret_pair(
                &format!("{prefix}.mas_delegation.shared_secret"),
                &mut mas.shared_secret,
                &mas.shared_secret_file,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let mut errors = ValidationErrors::new();
        AuthConfig::default().validate("auth", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn rejects_duplicate_idp_ids() {
        let mut cfg = AuthConfig::default();
        let make = |id: &str| OidcProviderConfig {
            idp_id: id.into(),
            idp_name: None,
            issuer: "https://idp.example".into(),
            client_id: "abc".into(),
            client_secret: SecretString::default(),
            client_secret_file: None,
            scopes: default_oidc_scopes(),
        };
        cfg.oidc_providers = vec![make("google"), make("google")];
        let mut errors = ValidationErrors::new();
        cfg.validate("auth", &mut errors);
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.message.contains("more than one provider"))
        );
    }

    #[test]
    fn rejects_mas_delegation_combined_with_oidc() {
        let mut cfg = AuthConfig::default();
        cfg.mas_delegation = Some(MasDelegationConfig {
            endpoint: "https://mas.example".into(),
            shared_secret: SecretString::from("x"),
            shared_secret_file: None,
        });
        cfg.oidc_providers = vec![OidcProviderConfig {
            idp_id: "google".into(),
            idp_name: None,
            issuer: "https://idp.example".into(),
            client_id: "abc".into(),
            client_secret: SecretString::default(),
            client_secret_file: None,
            scopes: default_oidc_scopes(),
        }];
        let mut errors = ValidationErrors::new();
        cfg.validate("auth", &mut errors);
        assert!(errors.0.iter().any(|e| e.path == "auth.mas_delegation"));
    }

    #[test]
    fn resolves_secrets_from_files() {
        let dir = std::env::temp_dir().join(format!("hs-config-auth-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret_path = dir.join("shared_secret");
        std::fs::write(&secret_path, "topsecret\n").unwrap();

        let mut cfg = AuthConfig::default();
        cfg.registration_shared_secret_file = Some(secret_path);
        cfg.resolve_secrets("auth").unwrap();
        assert_eq!(cfg.registration_shared_secret.as_str(), Some("topsecret"));
        std::fs::remove_dir_all(dir).ok();
    }
}
