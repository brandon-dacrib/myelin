//! Server identity: the homeserver's name and its Ed25519 signing key.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};

/// Server identity. Restart required to change (see [`crate::reload`]):
/// `server_name` is embedded in every user ID, room ID and event this
/// process has ever produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The domain in `@user:server_name`, room aliases and event origins.
    /// Corresponds to Synapse's `server_name`. Changing it after any room
    /// exists is not supported by any Matrix homeserver, including this
    /// one.
    pub server_name: String,

    /// The externally reachable base URL for clients, if different from
    /// `https://{server_name}`. Corresponds to Synapse's `public_baseurl`.
    #[serde(default)]
    pub public_baseurl: Option<String>,

    /// Directory holding this server's Ed25519 signing keys. Corresponds to
    /// Synapse's `signing_key_path` (a file here; a directory in our
    /// layout because multiple active keys are normal during rotation).
    #[serde(default = "default_signing_key_path")]
    pub signing_key_path: PathBuf,

    /// Contact address advertised for abuse reports and shown to operators
    /// of other servers. Corresponds to Synapse's `admin_contact`.
    #[serde(default)]
    pub admin_contact: Option<String>,

    /// Whether this server opts in to the anonymised statistics-reporting
    /// endpoint. Corresponds to Synapse's `report_stats`.
    #[serde(default)]
    pub report_stats: bool,
}

fn default_signing_key_path() -> PathBuf {
    PathBuf::from("./signing-keys")
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server_name: String::new(),
            public_baseurl: None,
            signing_key_path: default_signing_key_path(),
            admin_contact: None,
            report_stats: false,
        }
    }
}

impl Validate for ServerConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if self.server_name.trim().is_empty() {
            errors.push(format!("{prefix}.server_name"), "must not be empty");
        } else if self.server_name.chars().any(char::is_whitespace) {
            errors.push(
                format!("{prefix}.server_name"),
                format!("{:?} must not contain whitespace", self.server_name),
            );
        } else if self.server_name.len() > 255 {
            errors.push(
                format!("{prefix}.server_name"),
                "must be at most 255 characters (DNS name limit)",
            );
        }
        if let Some(url) = &self.public_baseurl
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            errors.push(
                format!("{prefix}.public_baseurl"),
                format!("{url:?} must start with http:// or https://"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace_names() {
        let mut errors = ValidationErrors::new();
        ServerConfig {
            server_name: "  ".into(),
            ..Default::default()
        }
        .validate("server", &mut errors);
        assert_eq!(errors.0[0].path, "server.server_name");
        assert_eq!(errors.0[0].message, "must not be empty");
    }

    #[test]
    fn rejects_baseurl_without_scheme() {
        let mut errors = ValidationErrors::new();
        ServerConfig {
            server_name: "example.org".into(),
            public_baseurl: Some("example.org".into()),
            ..Default::default()
        }
        .validate("server", &mut errors);
        assert_eq!(errors.0.len(), 1);
        assert!(errors.0[0].message.contains("http"));
    }

    #[test]
    fn accepts_a_normal_config() {
        let mut errors = ValidationErrors::new();
        ServerConfig {
            server_name: "matrix.example.org".into(),
            ..Default::default()
        }
        .validate("server", &mut errors);
        assert!(errors.is_empty());
    }
}
