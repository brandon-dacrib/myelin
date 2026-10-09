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
    /// Whether people may sign in with a password (`m.login.password`). Turned off, `/login`
    /// neither offers nor accepts it, so people sign in through single sign-on or the OAuth
    /// issuer, and signing in to this interface with a password stops working too (paste an
    /// access token instead). Re-entering a password to confirm a sensitive change, such as
    /// removing a device, still works, as with Synapse's `only_for_reauth`. Corresponds to
    /// Synapse's `password_config.enabled`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// A secret mixed into every password hash, so a copy of the database alone is not enough to
    /// guess passwords from. Set it once and keep it: changing or losing it makes every existing
    /// password stop working. Migrating from Synapse, set the same value Synapse had. Prefer
    /// `pepper_file`. Corresponds to Synapse's `password_config.pepper`.
    #[serde(default)]
    pub pepper: SecretString,
    /// Path to a file holding the pepper, read in place of `pepper` so the secret stays out of
    /// the database and its backups. Changing or losing it makes every existing password stop
    /// working, as for `pepper`.
    #[serde(default)]
    pub pepper_file: Option<PathBuf>,
    /// What a new password must contain. Checked when a password is set or changed, never
    /// against passwords people already have. Corresponds to Synapse's
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
    /// The fewest characters a new password may have. Length matters far more than character
    /// classes; at least 1.
    #[serde(default = "default_minimum_password_length")]
    pub minimum_length: u32,
    /// Whether a new password must contain a digit (0-9).
    #[serde(default)]
    pub require_digit: bool,
    /// Whether a new password must contain a symbol (a character that is not a letter or a
    /// digit).
    #[serde(default)]
    pub require_symbol: bool,
    /// Whether a new password must contain an uppercase letter.
    #[serde(default)]
    pub require_uppercase: bool,
    /// Whether a new password must contain a lowercase letter.
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

fn default_recaptcha_siteverify_api() -> String {
    "https://www.recaptcha.net/recaptcha/api/siteverify".to_owned()
}

/// A CAPTCHA (Google reCAPTCHA, or a service that answers its verification API) that people
/// solve when they sign up, to keep scripts from creating accounts in bulk. Corresponds to
/// Synapse's `recaptcha_*` settings and `enable_registration_captcha`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecaptchaConfig {
    /// Whether every new account must solve the CAPTCHA. Needs `public_key` and `private_key`.
    /// Off by default. With the keys set and this off, a client may still offer the CAPTCHA and
    /// the server checks it, but sign-up does not require it. Corresponds to Synapse's
    /// `enable_registration_captcha`.
    #[serde(default)]
    pub required: bool,
    /// The site key the CAPTCHA service gave this server, which clients show the puzzle with.
    /// Not a secret. Corresponds to Synapse's `recaptcha_public_key`.
    #[serde(default)]
    pub public_key: Option<String>,
    /// The secret key the CAPTCHA service gave this server, which this server checks answers
    /// with. Prefer `private_key_file`. Corresponds to Synapse's `recaptcha_private_key`.
    #[serde(default)]
    pub private_key: SecretString,
    /// Path to a file holding the secret key, read in place of `private_key`. Corresponds to
    /// Synapse's `recaptcha_private_key_path`.
    #[serde(default)]
    pub private_key_file: Option<PathBuf>,
    /// Where this server checks an answer. The default is Google's; change it only for a
    /// compatible service. Corresponds to Synapse's `recaptcha_siteverify_api`.
    #[serde(default = "default_recaptcha_siteverify_api")]
    pub siteverify_api: String,
}

impl Default for RecaptchaConfig {
    fn default() -> Self {
        Self {
            required: false,
            public_key: None,
            private_key: SecretString::default(),
            private_key_file: None,
            siteverify_api: default_recaptcha_siteverify_api(),
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
    /// The provider's issuer URL, from which this server discovers its endpoints and keys
    /// (`https://accounts.google.com`, `https://keycloak.example.org/realms/main`).
    pub issuer: String,
    /// The client ID the provider gave this server when it was registered there.
    pub client_id: String,
    /// The client secret the provider gave this server. Prefer `client_secret_file`.
    #[serde(default)]
    pub client_secret: SecretString,
    /// Path to a file holding the client secret, read in place of `client_secret`.
    #[serde(default)]
    pub client_secret_file: Option<PathBuf>,
    /// The OAuth scopes asked of the provider at sign-in. `openid` is required; `profile`
    /// brings the person's name, `email` their address.
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
}

fn default_oidc_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into()]
}

fn default_cas_idp_name() -> String {
    "CAS".to_owned()
}

/// Sign-in through a CAS server (Apereo CAS, the single sign-on many universities run).
/// Corresponds to Synapse's `cas_config`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CasConfig {
    /// The CAS server's address, the part before `/login` (`https://cas.example.edu/cas`).
    /// People are sent to `<server_url>/login` to sign in, and this server checks the ticket
    /// they come back with at `<server_url>/proxyValidate`. Corresponds to Synapse's
    /// `cas_config.server_url`.
    pub server_url: String,
    /// The address CAS sends people back to, when it is not `server.public_baseurl` (a server
    /// behind a proxy that CAS reaches by another name). Unset, `server.public_baseurl` is used.
    /// Many CAS servers only send people back to addresses registered with them, so this must
    /// match what the CAS administrator registered. Corresponds to Synapse's
    /// `cas_config.service_url`.
    #[serde(default)]
    pub service_url: Option<String>,
    /// The CAS attribute holding the person's name, used as the display name of an account
    /// created at their first sign-in (`displayName`, `cn`). Unset, a new account has no
    /// display name. Corresponds to Synapse's `cas_config.displayname_attribute`.
    #[serde(default)]
    pub displayname_attribute: Option<String>,
    /// Attributes a person must have to sign in, as `attribute: value` (`affiliation: staff`),
    /// or `attribute: null` to require only that the attribute is present. Somebody whose CAS
    /// account lacks one is refused. Empty by default: anyone CAS signs in may. Corresponds to
    /// Synapse's `cas_config.required_attributes`.
    #[serde(default)]
    pub required_attributes: std::collections::BTreeMap<String, Option<String>>,
    /// The name shown on the sign-in button ("Sign in with ..."). Corresponds to Synapse's
    /// `cas_config.idp_name`.
    #[serde(default = "default_cas_idp_name")]
    pub idp_name: String,
    /// The version of the CAS protocol the server speaks: `3` checks tickets at
    /// `<server_url>/p3/proxyValidate`, the CAS 3 endpoint that returns a person's attributes;
    /// `1` or `2`, or unset (the default), at `<server_url>/proxyValidate`, which CAS 3 servers
    /// answer too. Corresponds to Synapse's `cas_config.protocol_version`.
    #[serde(default)]
    pub protocol_version: Option<u8>,
    /// Whether a person CAS signs in for the first time gets an account here. Off, only people
    /// who already have an account -- linked at an earlier sign-in, or with the user name CAS
    /// gives them -- can sign in, and the rest see a page saying so. On by default. Corresponds
    /// to Synapse's `cas_config.enable_registration`.
    #[serde(default = "default_true")]
    pub enable_registration: bool,
    /// Whether a CAS user name made of digits only (a student number, `12345`) gets
    /// `numeric_ids_prefix` put in front of it (`@u12345:example.org`), so it cannot be mistaken
    /// for a guest's number. Off by default: the name is used as it is. Corresponds to Synapse's
    /// `cas_config.allow_numeric_ids`.
    #[serde(default)]
    pub allow_numeric_ids: bool,
    /// What goes in front of a digits-only CAS user name when `allow_numeric_ids` is on:
    /// letters and digits only. `u` by default. Choose it so it cannot collide with a name
    /// somebody already has (`1234` becomes `u1234`). Corresponds to Synapse's
    /// `cas_config.numeric_ids_prefix`.
    #[serde(default = "default_numeric_ids_prefix")]
    pub numeric_ids_prefix: String,
}

fn default_numeric_ids_prefix() -> String {
    "u".to_owned()
}

/// Settings shared by every single-sign-on provider (CAS today). Corresponds to Synapse's
/// `sso` section.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SsoConfig {
    /// Applications people are sent straight back to after signing in through single sign-on,
    /// without the page that asks them to confirm where they are going. Each entry is the start
    /// of an address (`https://app.element.io/`): an application whose return address begins
    /// with one is trusted. End each entry with a `/` after the host name, or
    /// `https://my.client` also trusts `https://my.client.evil.example`. Empty by default:
    /// everybody sees the confirmation page, which is what stops a link somebody else crafted
    /// from signing a person in to an application they never meant to use. Corresponds to
    /// Synapse's `sso.client_whitelist`.
    #[serde(default)]
    pub client_whitelist: Vec<String>,
    /// Whether a person's display name here follows the name the sign-on provider gives at
    /// each sign-in (`auth.cas.displayname_attribute`), so a name changed at the provider shows
    /// in their rooms after their next sign-in. Off by default: the provider's name is used
    /// only when the account is created, and the person keeps whatever name they set since.
    /// Corresponds to Synapse's `sso.update_profile_information`.
    #[serde(default)]
    pub update_profile_information: bool,
}

/// Matrix Authentication Service delegation mode: this server introspects
/// tokens against MAS instead of running its own OAuth issuer. Corresponds
/// to Synapse's `experimental_features.msc3861` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MasDelegationConfig {
    /// The URL at which this server reaches MAS to check tokens and provision accounts: MAS's
    /// internal address, not the one people sign in at.
    pub endpoint: String,
    /// Inline shared secret authenticating this server to MAS. Prefer
    /// `shared_secret_file`.
    #[serde(default)]
    pub shared_secret: SecretString,
    /// Path to a file holding the shared secret, read in place of `shared_secret`.
    #[serde(default)]
    pub shared_secret_file: Option<PathBuf>,
}

/// Who can sign up and how people sign in: registration, passwords, tokens and other sign-in
/// services.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Whether anyone may create an account on this server from a client (`POST /register`).
    /// Off by default: accounts are then made by an administrator or with a registration token
    /// or invite link (Settings). Turned on, anyone who can reach the server can sign up, so
    /// pair it with registration tokens or expect spam accounts. Corresponds to Synapse's
    /// `enable_registration`.
    #[serde(default)]
    pub enable_registration: bool,
    /// Let people use this server without an account: a client can ask for a guest session
    /// (`POST /register?kind=guest`) and gets a temporary account with no password. A guest can
    /// read rooms whose history is world-readable, join rooms whose guest access is set to "can
    /// join", talk there, and later turn the guest account into a full one by choosing a
    /// username and password. Guests cannot create rooms, invite people or upload files. Off by
    /// default: most servers only want people with accounts. Turning it off again stops new
    /// guest sessions; guests who already have one keep it. Corresponds to Synapse's
    /// `allow_guest_access`.
    #[serde(default)]
    pub allow_guest_access: bool,
    /// Identity servers this server may use to invite people by email address, as `host` or
    /// `host:port` (for example `vector.im` or `matrix.org`). When somebody invites an email
    /// address, this server asks the identity server their client names who owns it -- and,
    /// if nobody does yet, asks it to keep the invitation and email them. Only servers listed
    /// here are ever contacted, so a client cannot make this server send requests anywhere it
    /// likes. Empty by default: inviting by email address is then refused, and inviting by
    /// Matrix user ID works as always. Corresponds to Synapse's trust in the client-named
    /// `id_server`, which Synapse does not restrict.
    #[serde(default)]
    pub identity_servers: Vec<String>,
    /// A secret that lets a tool create accounts, administrators included, without signing in
    /// (the shared-secret registration that `hs register` and Synapse's `register_new_matrix_user`
    /// use). Anyone holding it can make an administrator, so leave it empty unless a script needs
    /// it. Prefer `registration_shared_secret_file`. Corresponds to Synapse's
    /// `registration_shared_secret`.
    #[serde(default)]
    pub registration_shared_secret: SecretString,
    /// Path to a file holding the shared-secret registration secret, read in place of
    /// `registration_shared_secret` so the secret stays out of the database.
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
    /// How long an access token from the OAuth issuer works before the client must refresh it.
    /// Shorter limits the damage of a leaked token; clients refresh on their own, so people do
    /// not notice. Tokens from the classic sign-in that cannot be refreshed are not affected,
    /// as in Synapse. Corresponds to Synapse's `access_token_lifetime`.
    #[serde(default = "default_access_token_lifetime")]
    pub access_token_lifetime: Duration,
    /// How long a refresh token works: after it, a device that has not been used must sign in
    /// again. Unset, refresh tokens never expire. Corresponds to Synapse's
    /// `refreshable_access_token_lifetime` family.
    #[serde(default = "default_refresh_token_lifetime")]
    pub refresh_token_lifetime: Option<Duration>,
    /// Password sign-in: whether it is offered, the pepper mixed into hashes, and what a new
    /// password must contain.
    #[serde(default)]
    pub password: PasswordConfig,
    /// A CAPTCHA people solve when they sign up. Unset keys (the default) mean no CAPTCHA.
    #[serde(default)]
    pub recaptcha: RecaptchaConfig,
    /// Other sign-in services people may use instead of a password here ("Sign in with Google",
    /// a company Keycloak or Okta), each registered with the provider first. Empty by default.
    /// Corresponds to Synapse's `oidc_providers`.
    #[serde(default)]
    pub oidc_providers: Vec<OidcProviderConfig>,
    /// Sign-in through a CAS server instead of, or as well as, a password here. Unset by default.
    /// People who sign in through CAS for the first time get an account named after their CAS
    /// user name; somebody whose CAS name matches an existing account signs in to that account.
    /// Needs `server.public_baseurl` (or `service_url`), since CAS sends people back there.
    /// Corresponds to Synapse's `cas_config`.
    #[serde(default)]
    pub cas: Option<CasConfig>,
    /// What happens after single sign-on, for every provider: `client_whitelist` lists the
    /// address prefixes of applications people are sent straight back to, without the page
    /// asking them to confirm (end each with a `/` after the host name). Empty by default.
    /// Corresponds to Synapse's `sso`.
    #[serde(default)]
    pub sso: SsoConfig,
    /// The domains a validation email's link may send people on to once they follow it (the
    /// `next_link` an application asks for, such as its own "you can go back now" page). Unset
    /// (the default), any `http` or `https` address is allowed; set, only addresses whose host
    /// is listed are (`[app.element.io]`), and an empty list allows none. An address on the
    /// person's own disk (`file:`) is never allowed. Corresponds to Synapse's
    /// `next_link_domain_whitelist`.
    #[serde(default)]
    pub next_link_domain_whitelist: Option<Vec<String>>,
    /// Hand sign-in to a separate Matrix Authentication Service (MAS) instead of this server's
    /// own OAuth issuer. Unset by default, which is right unless MAS is already deployed.
    /// Corresponds to Synapse's `experimental_features.msc3861`.
    #[serde(default)]
    pub mas_delegation: Option<MasDelegationConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enable_registration: false,
            allow_guest_access: false,
            identity_servers: Vec::new(),
            user_directory_search_all_users: false,
            registration_shared_secret: SecretString::default(),
            registration_shared_secret_file: None,
            access_token_lifetime: default_access_token_lifetime(),
            refresh_token_lifetime: default_refresh_token_lifetime(),
            password: PasswordConfig::default(),
            recaptcha: RecaptchaConfig::default(),
            oidc_providers: Vec::new(),
            cas: None,
            sso: SsoConfig::default(),
            next_link_domain_whitelist: None,
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
        let captcha = &self.recaptcha;
        if captcha.required
            && (captcha
                .public_key
                .as_deref()
                .is_none_or(|k| k.trim().is_empty())
                || (!captcha.private_key.is_some() && captcha.private_key_file.is_none()))
        {
            errors.push(
                format!("{prefix}.recaptcha.required"),
                "needs public_key and private_key (or private_key_file)",
            );
        }
        if !(captcha.siteverify_api.starts_with("https://")
            || captcha.siteverify_api.starts_with("http://"))
        {
            errors.push(
                format!("{prefix}.recaptcha.siteverify_api"),
                "must be an http:// or https:// URL",
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
        for (i, server) in self.identity_servers.iter().enumerate() {
            let server = server.trim();
            if server.is_empty() || server.contains('/') || server.contains("://") {
                errors.push(
                    format!("{prefix}.identity_servers[{i}]"),
                    "must be a host name, optionally with :port, such as vector.im",
                );
            }
        }
        if let Some(cas) = &self.cas {
            let url = cas.server_url.trim();
            if url.is_empty() {
                errors.push(format!("{prefix}.cas.server_url"), "must not be empty");
            } else if !(url.starts_with("https://") || url.starts_with("http://")) {
                errors.push(
                    format!("{prefix}.cas.server_url"),
                    "must be an http:// or https:// address",
                );
            }
            if let Some(service) = &cas.service_url
                && !(service.starts_with("https://") || service.starts_with("http://"))
            {
                errors.push(
                    format!("{prefix}.cas.service_url"),
                    "must be an http:// or https:// address",
                );
            }
            if cas.idp_name.trim().is_empty() {
                errors.push(format!("{prefix}.cas.idp_name"), "must not be empty");
            }
            if let Some(version) = cas.protocol_version
                && !(1..=3).contains(&version)
            {
                errors.push(
                    format!("{prefix}.cas.protocol_version"),
                    "must be 1, 2 or 3",
                );
            }
            if cas.numeric_ids_prefix.is_empty()
                || !cas
                    .numeric_ids_prefix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric())
            {
                errors.push(
                    format!("{prefix}.cas.numeric_ids_prefix"),
                    "must be letters and digits only",
                );
            }
        }
        for (i, client) in self.sso.client_whitelist.iter().enumerate() {
            if !(client.starts_with("https://") || client.starts_with("http://")) {
                errors.push(
                    format!("{prefix}.sso.client_whitelist[{i}]"),
                    "must be an http:// or https:// address",
                );
            }
        }
        for (i, domain) in self.next_link_domain_whitelist.iter().flatten().enumerate() {
            let domain = domain.trim();
            if domain.is_empty() || domain.contains('/') || domain.contains(':') {
                errors.push(
                    format!("{prefix}.next_link_domain_whitelist[{i}]"),
                    "must be a host name, such as app.element.io",
                );
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
            &format!("{prefix}.password.pepper"),
            &mut self.password.pepper,
            &self.password.pepper_file,
        )?;
        resolve_secret_pair(
            &format!("{prefix}.recaptcha.private_key"),
            &mut self.recaptcha.private_key,
            &self.recaptcha.private_key_file,
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
    fn a_required_captcha_needs_its_keys() {
        let mut cfg = AuthConfig::default();
        cfg.recaptcha.required = true;
        let mut errors = ValidationErrors::new();
        cfg.validate("auth", &mut errors);
        assert!(errors.0.iter().any(|e| e.path == "auth.recaptcha.required"));

        cfg.recaptcha.public_key = Some("site".into());
        cfg.recaptcha.private_key = SecretString::from("secret");
        let mut errors = ValidationErrors::new();
        cfg.validate("auth", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");

        cfg.recaptcha.siteverify_api = "ftp://nope".into();
        let mut errors = ValidationErrors::new();
        cfg.validate("auth", &mut errors);
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.path == "auth.recaptcha.siteverify_api")
        );
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
    fn cas_needs_a_server_url_that_is_an_http_address() {
        let mut cfg = AuthConfig::default();
        cfg.cas = Some(CasConfig {
            server_url: "cas.example.edu".into(),
            service_url: None,
            displayname_attribute: None,
            required_attributes: Default::default(),
            idp_name: default_cas_idp_name(),
            protocol_version: Some(4),
            enable_registration: true,
            allow_numeric_ids: true,
            numeric_ids_prefix: "u-".into(),
        });
        let mut errors = ValidationErrors::new();
        cfg.validate("auth", &mut errors);
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.path == "auth.cas.protocol_version")
        );
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.path == "auth.cas.numeric_ids_prefix")
        );
        assert!(errors.0.iter().any(|e| e.path == "auth.cas.server_url"));

        let cfg: AuthConfig = serde_json::from_value(
            serde_json::json!({"cas": {"server_url": "https://cas.example.edu/cas"}}),
        )
        .unwrap();
        let mut errors = ValidationErrors::new();
        cfg.validate("auth", &mut errors);
        assert!(errors.is_empty());
        assert_eq!(cfg.cas.unwrap().idp_name, "CAS");
    }

    #[test]
    fn sso_client_whitelist_and_next_link_domains_are_checked() {
        let cfg: AuthConfig = serde_json::from_value(serde_json::json!({
            "sso": {"client_whitelist": ["https://app.example/", "app.example"]},
            "next_link_domain_whitelist": ["app.example", "https://app.example", ""],
        }))
        .unwrap();
        let mut errors = ValidationErrors::new();
        cfg.validate("auth", &mut errors);
        let paths: Vec<&str> = errors.0.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "auth.sso.client_whitelist[1]",
                "auth.next_link_domain_whitelist[1]",
                "auth.next_link_domain_whitelist[2]",
            ]
        );
        let defaults = AuthConfig::default();
        assert!(defaults.sso.client_whitelist.is_empty());
        assert_eq!(defaults.next_link_domain_whitelist, None);
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
