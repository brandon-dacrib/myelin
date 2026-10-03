//! How this server reaches other hosts: the address families its outbound connections use.
//! Read per new connection by every outbound client (`hs_http::outbound`), so a change applies
//! to the running server at once.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};

const fn default_true() -> bool {
    true
}

/// How this server connects to other hosts: other Matrix servers, push gateways, bridges and
/// other appservices, identity servers, sign-in providers, and the sites it fetches link
/// previews from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Connections this server opens to other hosts.
    #[serde(default)]
    pub outbound: OutboundConfig,
}

/// The address policy of every connection this server opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutboundConfig {
    /// Connect to other servers over IPv4 only. On by default because many container networks
    /// have no IPv6 route, and a server whose IPv6 address is unreachable would otherwise fail
    /// to fetch from it. Turn it off on a host with working IPv6; the server then tries every
    /// address a name resolves to, and falls back to the next address when one does not
    /// connect. Applies to every connection this server opens: to other Matrix servers,
    /// push gateways, bridges, identity servers, sign-in providers and link previews.
    /// Synapse has no equivalent setting.
    #[serde(default = "default_true")]
    pub ipv4_only: bool,
}

impl Default for OutboundConfig {
    fn default() -> Self {
        Self { ipv4_only: true }
    }
}

impl Validate for NetworkConfig {
    fn validate(&self, _prefix: &str, _errors: &mut ValidationErrors) {
        // A boolean has nothing to validate; the impl keeps every section on the same path.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_only_is_the_default() {
        assert!(NetworkConfig::default().outbound.ipv4_only);
        let parsed: NetworkConfig = serde_yaml_ng::from_str("outbound: {}").unwrap();
        assert!(parsed.outbound.ipv4_only);
        let parsed: NetworkConfig =
            serde_yaml_ng::from_str("outbound:\n  ipv4_only: false\n").unwrap();
        assert!(!parsed.outbound.ipv4_only);
    }
}
