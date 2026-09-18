//! HTTP listeners: bind addresses, TLS and which resource families each
//! socket serves. Corresponds to Synapse's `listeners` list.

use std::collections::HashSet;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};

/// A resource family a listener can serve. Synapse calls these `resources`
/// with `names` like `client`, `federation`, `media`, `metrics`; we keep
/// the same vocabulary since it is what operators already know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListenerResource {
    /// `/_matrix/client/*` and legacy `/_matrix/r0/*`.
    Client,
    /// `/_matrix/federation/*`, `/_matrix/key/*`.
    Federation,
    /// `/_matrix/media/*`.
    Media,
    /// Prometheus text exposition.
    Metrics,
    /// The native `/api/v1` admin API and the embedded management web UI.
    Admin,
    /// `/health` liveness/readiness only, no auth, for load balancers.
    Health,
}

/// TLS material for a listener.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM certificate chain path.
    pub certificate_path: PathBuf,
    /// PEM private key path.
    pub private_key_path: PathBuf,
}

/// One HTTP listener.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    /// Addresses to bind. Corresponds to Synapse's `bind_addresses`.
    #[serde(default = "default_bind_addresses")]
    pub bind_addresses: Vec<String>,

    /// TCP port.
    pub port: u16,

    /// TLS material, or `None` to serve plaintext (typically behind a
    /// reverse proxy terminating TLS).
    #[serde(default)]
    pub tls: Option<TlsConfig>,

    /// Resource families this listener serves. Corresponds to Synapse's
    /// `resources[].names`.
    pub resources: Vec<ListenerResource>,

    /// Trust `X-Forwarded-For` and `X-Forwarded-Proto` from this listener's
    /// peers. Corresponds to Synapse's `x_forwarded`.
    #[serde(default)]
    pub x_forwarded: bool,
}

fn default_bind_addresses() -> Vec<String> {
    vec!["::".to_owned()]
}

/// All configured listeners. Restart required to change (see
/// [`crate::reload`]): sockets are bound once at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListenersConfig {
    /// One entry per bound socket.
    #[serde(default = "default_listeners")]
    pub listeners: Vec<Listener>,
}

fn default_listeners() -> Vec<Listener> {
    vec![Listener {
        bind_addresses: default_bind_addresses(),
        port: 8008,
        tls: None,
        resources: vec![
            ListenerResource::Client,
            ListenerResource::Federation,
            ListenerResource::Media,
            ListenerResource::Health,
        ],
        x_forwarded: false,
    }]
}

impl Default for ListenersConfig {
    fn default() -> Self {
        Self {
            listeners: default_listeners(),
        }
    }
}

impl Validate for ListenersConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if self.listeners.is_empty() {
            errors.push(
                format!("{prefix}.listeners"),
                "at least one listener is required",
            );
            return;
        }
        let mut seen: HashSet<(String, u16)> = HashSet::new();
        for (i, l) in self.listeners.iter().enumerate() {
            let path = format!("{prefix}.listeners[{i}]");
            if l.resources.is_empty() {
                errors.push(
                    format!("{path}.resources"),
                    "must serve at least one resource",
                );
            }
            if l.bind_addresses.is_empty() {
                errors.push(
                    format!("{path}.bind_addresses"),
                    "must list at least one address",
                );
            }
            if l.port == 0 {
                errors.push(
                    format!("{path}.port"),
                    "0 is not a bindable port; choose an explicit port",
                );
            }
            for addr in &l.bind_addresses {
                let key = (addr.clone(), l.port);
                if !seen.insert(key) {
                    errors.push(
                        path.clone(),
                        format!("{addr}:{} is also bound by another listener", l.port),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_listener_list() {
        let mut errors = ValidationErrors::new();
        ListenersConfig { listeners: vec![] }.validate("listeners", &mut errors);
        assert_eq!(errors.0[0].message, "at least one listener is required");
    }

    #[test]
    fn rejects_duplicate_bind() {
        let l = Listener {
            bind_addresses: vec!["0.0.0.0".into()],
            port: 8008,
            tls: None,
            resources: vec![ListenerResource::Client],
            x_forwarded: false,
        };
        let mut errors = ValidationErrors::new();
        ListenersConfig {
            listeners: vec![l.clone(), l],
        }
        .validate("listeners", &mut errors);
        assert!(errors.0.iter().any(|e| e.message.contains("also bound")));
    }

    #[test]
    fn rejects_listener_with_no_resources() {
        let mut errors = ValidationErrors::new();
        ListenersConfig {
            listeners: vec![Listener {
                bind_addresses: default_bind_addresses(),
                port: 8008,
                tls: None,
                resources: vec![],
                x_forwarded: false,
            }],
        }
        .validate("listeners", &mut errors);
        assert_eq!(errors.0[0].path, "listeners.listeners[0].resources");
    }

    #[test]
    fn default_is_valid() {
        let mut errors = ValidationErrors::new();
        ListenersConfig::default().validate("listeners", &mut errors);
        assert!(errors.is_empty());
    }
}
