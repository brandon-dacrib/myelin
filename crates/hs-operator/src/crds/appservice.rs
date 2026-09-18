//! The `AppService` custom resource: registers one Matrix application service (bridge or bot)
//! against a `Homeserver`, the CRD-native equivalent of dropping a registration YAML file into
//! `hs_config::appservice::AppservicesConfig::registration_files` (owned by track 13) or calling
//! track 11's registry API. The operator's job for this kind is to keep that registration in sync
//! with the referenced `Homeserver`, not to run any workload itself — an `AppService` describes a
//! registration, a [`crate::crds::bridge::Bridge`] describes the process that uses it.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{OperatorStatus, SecretKeyRef};

/// Namespace patterns an appservice claims, mirroring the Matrix appservice registration YAML's
/// `namespaces` block (`refs/matrix-spec` appservice API, and `docs/rfcs/*` for this project's own
/// registry shape).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NamespacePatterns {
    /// User ID regexes this appservice owns.
    #[serde(default)]
    pub users: Vec<NamespacePattern>,
    /// Room alias regexes.
    #[serde(default)]
    pub aliases: Vec<NamespacePattern>,
    /// Room ID regexes.
    #[serde(default)]
    pub rooms: Vec<NamespacePattern>,
}

/// One namespace regex entry.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NamespacePattern {
    /// The regular expression.
    pub regex: String,
    /// Whether this appservice exclusively owns matching IDs (no other appservice or local user
    /// may claim one).
    #[serde(default)]
    pub exclusive: bool,
}

/// `spec` of an `AppService`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, CustomResource)]
#[kube(
    group = "hs.matrix.org",
    version = "v1alpha1",
    kind = "AppService",
    namespaced,
    shortname = "as",
    status = "OperatorStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Homeserver","type":"string","jsonPath":".spec.homeserverRef"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AppServiceSpec {
    /// The name of the [`super::homeserver::Homeserver`] this registration applies to (same
    /// namespace).
    pub homeserver_ref: String,
    /// The appservice's stable ID (Synapse's/this project's `id` registration field).
    pub id: String,
    /// The URL this server pushes events to.
    pub url: String,
    /// `Secret` key holding the appservice's own token (used by the appservice to authenticate
    /// to the homeserver, `as_token`).
    pub as_token_secret_ref: SecretKeyRef,
    /// `Secret` key holding the homeserver's token (used by the homeserver to authenticate to the
    /// appservice, `hs_token`).
    pub hs_token_secret_ref: SecretKeyRef,
    /// The localpart of the appservice's sender user (`@<sender_localpart>:server_name`).
    pub sender_localpart: String,
    /// Namespaces this appservice claims.
    #[serde(default)]
    pub namespaces: NamespacePatterns,
    /// Whether this appservice is subject to the homeserver's normal rate limiting.
    #[serde(default)]
    pub rate_limited: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AppServiceSpec {
        AppServiceSpec {
            homeserver_ref: "main".to_owned(),
            id: "irc-bridge".to_owned(),
            url: "http://irc-bridge.default.svc:29999".to_owned(),
            as_token_secret_ref: SecretKeyRef {
                name: "irc-bridge-tokens".to_owned(),
                key: "as_token".to_owned(),
            },
            hs_token_secret_ref: SecretKeyRef {
                name: "irc-bridge-tokens".to_owned(),
                key: "hs_token".to_owned(),
            },
            sender_localpart: "ircbot".to_owned(),
            namespaces: NamespacePatterns {
                users: vec![NamespacePattern {
                    regex: "@irc_.*".to_owned(),
                    exclusive: true,
                }],
                aliases: vec![],
                rooms: vec![],
            },
            rate_limited: false,
        }
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = sample();
        let json = serde_json::to_value(&spec).unwrap();
        let back: AppServiceSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.id, spec.id);
        assert_eq!(back.namespaces.users.len(), 1);
    }

    #[test]
    fn spec_round_trips_through_yaml() {
        let spec = sample();
        let yaml = serde_yaml_ng::to_string(&spec).unwrap();
        let back: AppServiceSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back.url, spec.url);
    }
}
