//! Federation policy: reachability rules, allow/deny lists and outbound
//! transport tuning. Read at startup; see [`crate::reload`] for what a running server re-reads.

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

const fn default_max_queued_pdus_per_destination() -> u32 {
    10_000
}

const fn default_max_queued_durable_edus_per_destination() -> u32 {
    10_000
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
    /// Whether this server talks to other Matrix servers at all. Off, its users can only talk to
    /// each other: no joining rooms elsewhere, no messages from other servers. Corresponds to
    /// Synapse's `federation_domain_whitelist` set to an empty list.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// If set, federation traffic is restricted to exactly these server
    /// names. Corresponds to Synapse's `federation_domain_whitelist`.
    /// Checked on every outbound request; a change applies to the running
    /// server at once.
    #[serde(default)]
    pub domain_allowlist: Option<Vec<String>>,

    /// IP ranges (CIDR) federation requests must not be sent to.
    /// Corresponds to Synapse's `federation_ip_range_blacklist`.
    /// A change applies to the running server at once.
    #[serde(default = "default_ip_range_blocklist")]
    pub ip_range_blocklist: Vec<String>,

    /// IP ranges exempted from `ip_range_blocklist` (for federating with a
    /// deliberately private deployment). Corresponds to Synapse's
    /// `federation_ip_range_whitelist`. A change applies to the running
    /// server at once.
    #[serde(default)]
    pub ip_range_allowlist: Vec<String>,

    /// Verify TLS certificates on outbound federation requests.
    /// Corresponds to Synapse's `federation_verify_certificates`.
    ///
    /// Leave this `true` in production: setting it `false` makes outbound federation TLS accept
    /// *any* certificate, which is trivially machine-in-the-middled. It exists for test
    /// deployments and conformance harnesses (Complement and similar) that terminate TLS with a
    /// certificate this server has no other way to trust yet. The outbound client logs a
    /// prominent startup warning whenever this is `false`, precisely so it cannot go unnoticed in
    /// a real deployment. Prefer [`Self::custom_ca_certificates`] instead, if the actual goal is
    /// federating with one specific server whose certificate chains to a CA this server does not
    /// already trust — that trusts exactly the named CA, not every certificate on the internet.
    #[serde(default = "default_true")]
    pub verify_certificates: bool,

    /// Paths to additional PEM-encoded CA certificate files trusted for outbound federation TLS,
    /// on top of (never instead of) the ~140 public root CAs this server trusts by default.
    /// Corresponds to Synapse's `federation_custom_ca_list`. This is the answer to "how do I
    /// federate with a server whose certificate was issued by a CA that is not one of the public
    /// roots" without resorting to [`Self::verify_certificates`], which would trust every
    /// certificate rather than just the one CA actually meant: name the CA's certificate file
    /// here. A conformance harness's generated CA (Complement) and an internal deployment's
    /// private CA are both meant to be configured this way.
    #[serde(default)]
    pub custom_ca_certificates: Vec<String>,

    /// Whether outbound federation TLS also trusts whatever CA store the *operating system*
    /// trusts, in addition to this server's bundled public root CAs. Defaults to `false`.
    ///
    /// Trusting the OS store is the right choice for some deployments: an administrator who runs
    /// `update-ca-certificates` (or the platform equivalent) to add a corporate or internal CA
    /// reasonably expects every TLS client on that machine, including this one, to honour it
    /// automatically, and it is what many other pieces of server software do by default. It is
    /// the wrong choice as this *server's* unconditional default, though: outbound federation
    /// traffic authenticates events between servers that never agreed on a shared root of trust
    /// ahead of time (unlike, say, an internal service mesh with its own CA hierarchy), so
    /// silently broadening federation's trust to include every CA some unrelated piece of
    /// installed software, corporate TLS-inspecting proxy, or forgotten test certificate has
    /// added to the OS store is a real, if quiet, security regression for exactly the traffic
    /// this setting controls — and it is a regression the operator of *this* server may not even
    /// have chosen (the OS store can be broadened by anyone with root on the machine, for reasons
    /// having nothing to do with running a homeserver). Defaulting to `false` and pairing it with
    /// [`Self::custom_ca_certificates`] for the explicit, narrow case (name exactly the CA meant
    /// to be trusted) keeps that choice with the person configuring federation, not with whoever
    /// last ran an unrelated `update-ca-certificates`.
    #[serde(default)]
    pub trust_os_root_store: bool,

    /// How long this server waits for another server to answer one request before giving up
    /// and counting it as a failure. Too short fails slow but working servers; too long ties
    /// up a sender on a server that is gone. Corresponds to Synapse's
    /// `federation_client_timeout`.
    #[serde(default = "default_client_timeout")]
    pub client_timeout: Duration,

    /// The longest this server waits between attempts to reach a server that keeps failing.
    /// The wait doubles after each failure up to this; Reset backoff on the destination's page
    /// tries at once. Corresponds to Synapse's `destination_min_retry_interval` family,
    /// simplified to one ceiling.
    #[serde(default = "default_max_retry_backoff")]
    pub max_retry_backoff: Duration,

    /// How many events the outbound queue holds for one destination before it is dropped and
    /// the destination, once it answers again, is caught up with the latest event of each room
    /// it is behind in instead (it fetches the rest itself). Bounds what a server that is down
    /// for days costs this one's database. Corresponds to Synapse's catch-up mode
    /// (`destination_rooms`), which Synapse enters on the first failure; Synapse has no
    /// setting for it. At least 1.
    #[serde(default = "default_max_queued_pdus_per_destination")]
    pub max_queued_pdus_per_destination: u32,

    /// How many to-device messages and device-list and cross-signing key updates this server
    /// keeps waiting for one other server before it drops the oldest. They are kept on disk
    /// until that server accepts them, so a server that is down for a while still gets the
    /// encryption keys and device changes it missed when it is back; this bounds what one that
    /// never comes back costs. A dropped update is logged; the other server re-learns a user's
    /// devices on their next change or when one of its users asks. Synapse keeps them without
    /// a bound and has no setting for it. At least 1; a change applies to the next update queued.
    #[serde(default = "default_max_queued_durable_edus_per_destination")]
    pub max_queued_durable_edus_per_destination: u32,

    /// Whether other servers may read this server's public room directory, so their users can
    /// find this server's public rooms by browsing it. Off by default, as in Synapse.
    /// Corresponds to Synapse's `allow_public_rooms_over_federation`.
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
            custom_ca_certificates: Vec::new(),
            trust_os_root_store: false,
            client_timeout: default_client_timeout(),
            max_retry_backoff: default_max_retry_backoff(),
            max_queued_pdus_per_destination: default_max_queued_pdus_per_destination(),
            max_queued_durable_edus_per_destination:
                default_max_queued_durable_edus_per_destination(),
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
        for (i, path) in self.custom_ca_certificates.iter().enumerate() {
            if path.trim().is_empty() {
                errors.push(
                    format!("{prefix}.custom_ca_certificates[{i}]"),
                    "must not be empty",
                );
            }
        }
        if self.client_timeout.is_zero() {
            errors.push(format!("{prefix}.client_timeout"), "must be greater than 0");
        }
        if self.max_queued_pdus_per_destination == 0 {
            errors.push(
                format!("{prefix}.max_queued_pdus_per_destination"),
                "must be at least 1",
            );
        }
        if self.max_queued_durable_edus_per_destination == 0 {
            errors.push(
                format!("{prefix}.max_queued_durable_edus_per_destination"),
                "must be at least 1",
            );
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
    fn custom_ca_certificates_and_trust_os_root_store_default_off() {
        let cfg = FederationConfig::default();
        assert!(cfg.custom_ca_certificates.is_empty());
        assert!(!cfg.trust_os_root_store);
        assert!(cfg.verify_certificates);
    }

    #[test]
    fn rejects_an_empty_custom_ca_certificate_path() {
        let mut cfg = FederationConfig::default();
        cfg.custom_ca_certificates = vec!["  ".into()];
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        assert_eq!(errors.0[0].path, "federation.custom_ca_certificates[0]");
    }

    #[test]
    fn a_zero_queue_bound_is_rejected() {
        let mut cfg = FederationConfig::default();
        assert_eq!(cfg.max_queued_pdus_per_destination, 10_000);
        cfg.max_queued_pdus_per_destination = 0;
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        assert_eq!(
            errors.0[0].path,
            "federation.max_queued_pdus_per_destination"
        );
    }

    #[test]
    fn a_zero_durable_edu_bound_is_rejected() {
        let mut cfg = FederationConfig::default();
        assert_eq!(cfg.max_queued_durable_edus_per_destination, 10_000);
        cfg.max_queued_durable_edus_per_destination = 0;
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        assert_eq!(
            errors.0[0].path,
            "federation.max_queued_durable_edus_per_destination"
        );
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
