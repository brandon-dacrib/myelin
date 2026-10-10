//! Federation policy: reachability rules, allow/deny lists and outbound
//! transport tuning. Read at startup; see [`crate::reload`] for what a running server re-reads.

use std::collections::BTreeMap;

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

fn default_key_fetch_timeout() -> Duration {
    Duration::from_secs(10)
}

const fn default_max_queued_pdus_per_destination() -> u32 {
    10_000
}

const fn default_max_queued_durable_edus_per_destination() -> u32 {
    10_000
}

fn default_forget_unused_destinations_after() -> Duration {
    Duration::from_secs(7 * 24 * 60 * 60)
}

/// The notary key matrix.org publishes for its notary answers (`ed25519:auto`), as Synapse's
/// own default `trusted_key_servers` spells it (`synapse/config/key.py`).
pub const MATRIX_ORG_NOTARY_KEY_ID: &str = "ed25519:auto";
/// See [`MATRIX_ORG_NOTARY_KEY_ID`].
pub const MATRIX_ORG_NOTARY_KEY: &str = "Noi6WqcDj0QmPxCNQqgezwTlBKrfqehY1u2FyWP9uYw";

fn default_trusted_key_servers() -> Vec<TrustedKeyServer> {
    vec![TrustedKeyServer {
        server_name: "matrix.org".to_owned(),
        verify_keys: BTreeMap::from([(
            MATRIX_ORG_NOTARY_KEY_ID.to_owned(),
            MATRIX_ORG_NOTARY_KEY.to_owned(),
        )]),
    }]
}

/// A notary (Synapse: "trusted key server", "perspectives server") this server asks for
/// another server's signing keys when that server no longer publishes them, and the keys the
/// notary's answers must be signed with. See [`FederationConfig::trusted_key_servers`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrustedKeyServer {
    /// The notary's server name (`matrix.org`).
    pub server_name: String,
    /// The notary's own verify keys, `key_id` (`ed25519:auto`) to the base64 public key, as
    /// `GET /_matrix/key/v2/server` on the notary publishes them. An answer not signed by one
    /// of these is refused: without them a notary's answer is only as trustworthy as the
    /// connection it came over. At least one.
    pub verify_keys: BTreeMap<String, String>,
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

    /// How long one fetch of another server's signing keys (its `/_matrix/key/v2/server`, or
    /// one query of a notary) may take, connecting and answering together, before it is given
    /// up and the server is left alone for a while (a minute, doubling to an hour while it
    /// keeps failing). Shorter than `client_timeout` on purpose: a room's events cite many
    /// servers, some gone for good, and each gone server costs one of these while a join or a
    /// backfill verifies them. Synapse asks with 10 s too. A change applies to the next fetch.
    #[serde(default = "default_key_fetch_timeout")]
    pub key_fetch_timeout: Duration,

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

    /// How long a server this one shares no room with is kept on the Federation page before it
    /// is forgotten. Every server this one ever sent to is remembered with its retry state; once
    /// no room brings the two together it is only state, and nothing will be sent to it until a
    /// room does again (decision 0042). A sweep runs every hour and forgets each such server once
    /// it has had nothing queued and nothing happen (no attempt, no success, no failure) for
    /// this long, and each one failing for this long whose queued events are only for rooms
    /// this server has since left (leaving a large room leaves one row per server that was in
    /// it, most of them never answering). A server this one still shares a room with is never
    /// swept. `0` turns the sweep off; the Federation page's Forget and Prune do the same by
    /// hand at any time. Synapse keeps every destination for ever and has no setting for it. A
    /// change applies to the next sweep.
    #[serde(default = "default_forget_unused_destinations_after")]
    pub forget_unused_destinations_after: Duration,

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

    /// Notaries this server asks for another server's signing keys when that server does not
    /// publish them any more, or cannot be reached. A room that has existed for years has its
    /// creation and early state signed with keys its server has since rotated out, and most
    /// servers do not publish their retired keys; without a notary, which keeps every key it
    /// has ever fetched, such a room cannot be joined (the key the `m.room.create` event is
    /// signed with cannot be found). Each notary's answer must be signed by one of the
    /// `verify_keys` named for it and by the server the keys belong to, the Matrix
    /// specification's rule for notary answers.
    ///
    /// Defaults to matrix.org with its published notary key, as Synapse does. An empty list
    /// turns notary lookups off: a key the server itself does not publish is then not found.
    /// Corresponds to Synapse's `trusted_key_servers`. A change applies to the running server
    /// at once: the next key the server does not hold is asked of the new list.
    #[serde(default = "default_trusted_key_servers")]
    pub trusted_key_servers: Vec<TrustedKeyServer>,
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
            key_fetch_timeout: default_key_fetch_timeout(),
            max_queued_pdus_per_destination: default_max_queued_pdus_per_destination(),
            max_queued_durable_edus_per_destination:
                default_max_queued_durable_edus_per_destination(),
            forget_unused_destinations_after: default_forget_unused_destinations_after(),
            allow_public_rooms_over_federation: false,
            allow_device_name_lookup_over_federation: false,
            trusted_key_servers: default_trusted_key_servers(),
        }
    }
}

/// Whether `key` looks like an unpadded (or padded) standard-base64 Ed25519 public key: 32
/// bytes, so 43 characters (44 with one `=`). The decode itself happens where the key is used
/// (`hs-federation`); this catches a key pasted with its `ed25519:` prefix, a truncated one, or
/// one in the URL-safe alphabet at configuration time.
fn looks_like_base64_ed25519_key(key: &str) -> bool {
    let unpadded = key.strip_suffix('=').unwrap_or(key);
    unpadded.len() == 43
        && unpadded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
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
        if self.key_fetch_timeout.is_zero() {
            errors.push(
                format!("{prefix}.key_fetch_timeout"),
                "must be greater than 0",
            );
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
        for (i, notary) in self.trusted_key_servers.iter().enumerate() {
            let at = format!("{prefix}.trusted_key_servers[{i}]");
            if notary.server_name.trim().is_empty() {
                errors.push(format!("{at}.server_name"), "must not be empty");
            }
            if notary.verify_keys.is_empty() {
                errors.push(
                    format!("{at}.verify_keys"),
                    "must name at least one of the notary's verify keys (`ed25519:<version>`: \
                     base64 public key), as its /_matrix/key/v2/server publishes them; an \
                     answer this server cannot check the signature of is worth nothing",
                );
            }
            for (key_id, key) in &notary.verify_keys {
                if !key_id.starts_with("ed25519:") || key_id.len() <= "ed25519:".len() {
                    errors.push(
                        format!("{at}.verify_keys.{key_id}"),
                        "a key id is `ed25519:<version>`",
                    );
                }
                if !looks_like_base64_ed25519_key(key) {
                    errors.push(
                        format!("{at}.verify_keys.{key_id}"),
                        "the value is the base64 (standard alphabet, 43 characters) Ed25519 \
                         public key, as the notary's /_matrix/key/v2/server publishes it",
                    );
                }
            }
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
    fn the_default_notary_is_matrix_org_with_its_published_key() {
        let cfg = FederationConfig::default();
        assert_eq!(cfg.trusted_key_servers.len(), 1);
        let notary = &cfg.trusted_key_servers[0];
        assert_eq!(notary.server_name, "matrix.org");
        assert_eq!(
            notary
                .verify_keys
                .get(MATRIX_ORG_NOTARY_KEY_ID)
                .map(String::as_str),
            Some(MATRIX_ORG_NOTARY_KEY)
        );
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn an_empty_notary_list_turns_notary_lookups_off_and_is_valid() {
        let cfg: FederationConfig =
            serde_yaml_ng::from_str("trusted_key_servers: []").expect("parses");
        assert!(cfg.trusted_key_servers.is_empty());
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn a_notary_without_keys_or_with_a_malformed_key_is_rejected() {
        let cfg: FederationConfig = serde_yaml_ng::from_str(
            "trusted_key_servers:\n\
             \x20 - server_name: notary.example.org\n\
             \x20   verify_keys: {}\n\
             \x20 - server_name: \"\"\n\
             \x20   verify_keys:\n\
             \x20     \"auto\": \"not base64!\"\n\
             \x20     \"ed25519:ok\": \"Noi6WqcDj0QmPxCNQqgezwTlBKrfqehY1u2FyWP9uYw\"\n",
        )
        .expect("parses");
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        let paths: Vec<&str> = errors.0.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "federation.trusted_key_servers[0].verify_keys",
                "federation.trusted_key_servers[1].server_name",
                "federation.trusted_key_servers[1].verify_keys.auto",
                "federation.trusted_key_servers[1].verify_keys.auto",
            ],
            "{errors:?}"
        );
    }

    #[test]
    fn the_key_fetch_timeout_defaults_to_ten_seconds_and_must_not_be_zero() {
        let cfg = FederationConfig::default();
        assert_eq!(cfg.key_fetch_timeout, Duration::from_secs(10));
        let cfg: FederationConfig =
            serde_yaml_ng::from_str("key_fetch_timeout: 3s").expect("parses");
        assert_eq!(cfg.key_fetch_timeout, Duration::from_secs(3));
        let mut cfg = FederationConfig::default();
        cfg.key_fetch_timeout = Duration::ZERO;
        let mut errors = ValidationErrors::new();
        cfg.validate("federation", &mut errors);
        assert!(
            errors
                .0
                .iter()
                .any(|e| e.path == "federation.key_fetch_timeout"),
            "{errors:?}"
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
