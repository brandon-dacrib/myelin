//! Federation policy: reachability rules, allow/deny lists and outbound
//! transport tuning. Hot-reloadable (see [`crate::reload`]).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Duration;
use crate::error::{Validate, ValidationErrors};

const fn default_true() -> bool {
    true
}

fn default_client_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_max_retry_backoff() -> Duration {
    Duration::from_mins(60)
}

fn default_ip_range_blocklist() -> Vec<String> {
    vec![
        "127.0.0.0/8".into(),
        "10.0.0.0/8".into(),
        "172.16.0.0/12".into(),
        "192.168.0.0/16".into(),
        "100.64.0.0/10".into(),
        "169.254.0.0/16".into(),
        "::1/128".into(),
        "fe80::/10".into(),
        "fc00::/7".into(),
    ]
}

/// Federation reachability and transport policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FederationConfig {
    /// Master switch for outbound and inbound federation traffic.
    /// Corresponds to Synapse's `federation_domain_whitelist` being
    /// unset/set combined with the general notion of "federation off".
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// If set, federation traffic is restricted to exactly these server
    /// names. Corresponds to Synapse's `federation_domain_whitelist`.
    #[serde(default)]
    pub domain_allowlist: Option<Vec<String>>,

    /// IP ranges (CIDR) federation requests must not be sent to.
    /// Corresponds to Synapse's `federation_ip_range_blacklist`.
    #[serde(default = "default_ip_range_blocklist")]
    pub ip_range_blocklist: Vec<String>,

    /// IP ranges exempted from `ip_range_blocklist` (for federating with a
    /// deliberately private deployment). Corresponds to Synapse's
    /// `federation_ip_range_whitelist`.
    #[serde(default)]
    pub ip_range_allowlist: Vec<String>,

    /// Verify TLS certificates on outbound federation requests.
    /// Corresponds to Synapse's `federation_verify_certificates`.
    #[serde(default = "default_true")]
    pub verify_certificates: bool,

    /// Per-request timeout for outbound federation HTTP calls. Corresponds
    /// to Synapse's `federation_client_timeout`.
    #[serde(default = "default_client_timeout")]
    pub client_timeout: Duration,

    /// Cap on the exponential backoff between retries of a failed
    /// federation destination. Corresponds to Synapse's
    /// `destination_min_retry_interval` family, simplified to one ceiling.
    #[serde(default = "default_max_retry_backoff")]
    pub max_retry_backoff: Duration,

    /// Advertise this room's public directory over federation. Corresponds
    /// to Synapse's `allow_public_rooms_over_federation`.
    #[serde(default)]
    pub allow_public_rooms_over_federation: bool,

    /// Answer remote servers' `/_matrix/federation/*/user/devices/*`
    /// queries for device display names. Corresponds to Synapse's
    /// `allow_device_name_lookup_over_federation`.
    #[serde(default)]
    pub allow_device_name_lookup_over_federation: bool,
}

impl Default for FederationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            domain_allowlist: None,
            ip_range_blocklist: default_ip_range_blocklist(),
            ip_range_allowlist: Vec::new(),
            verify_certificates: true,
            client_timeout: default_client_timeout(),
            max_retry_backoff: default_max_retry_backoff(),
            allow_public_rooms_over_federation: false,
            allow_device_name_lookup_over_federation: false,
        }
    }
}

fn validate_cidr_list(prefix: &str, field: &str, list: &[String], errors: &mut ValidationErrors) {
    for (i, cidr) in list.iter().enumerate() {
        let (addr, plen) = match cidr.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (cidr.as_str(), None),
        };
        let Ok(ip) = addr.parse::<std::net::IpAddr>() else {
            errors.push(
                format!("{prefix}.{field}[{i}]"),
                format!("{cidr:?} is not a valid CIDR"),
            );
            continue;
        };
        if let Some(p) = plen {
            let max = if ip.is_ipv4() { 32 } else { 128 };
            match p.parse::<u8>() {
                Ok(bits) if bits <= max => {}
                _ => errors.push(
                    format!("{prefix}.{field}[{i}]"),
                    format!("{cidr:?} has an invalid prefix length (0..={max})"),
                ),
            }
        }
    }
}

impl Validate for FederationConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if let Some(list) = &self.domain_allowlist
            && list.is_empty()
        {
            errors.push(
                format!("{prefix}.domain_allowlist"),
                "an empty list blocks all federation; omit the key entirely to allow all servers",
            );
        }
        validate_cidr_list(
            prefix,
            "ip_range_blocklist",
            &self.ip_range_blocklist,
            errors,
        );
        validate_cidr_list(
            prefix,
            "ip_range_allowlist",
            &self.ip_range_allowlist,
            errors,
        );
        if self.client_timeout.is_zero() {
            errors.push(format!("{prefix}.client_timeout"), "must be greater than 0");
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        let mut errors = ValidationErrors::new();
        FederationConfig::default().validate("federation", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn empty_allowlist_is_rejected_with_a_helpful_message() {
        let mut cfg = FederationConfig::default();
        cfg.domain_allowlist = Some(vec![]);
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        assert_eq!(errors.0.len(), 1);
        assert!(errors.0[0].message.contains("blocks all federation"));
    }

    #[test]
    fn rejects_malformed_cidr_in_blocklist() {
        let mut cfg = FederationConfig::default();
        cfg.ip_range_blocklist = vec!["definitely-not-a-cidr".into()];
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        assert_eq!(errors.0[0].path, "federation.ip_range_blocklist[0]");
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let mut cfg = FederationConfig::default();
        cfg.client_timeout = Duration::ZERO;
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.path == "federation.client_timeout")
        );
    }
}
