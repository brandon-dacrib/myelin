//! The reload boundary: when each setting takes effect.
//!
//! Every setting in [`Config`] is exactly one of three kinds, listed in [`SETTINGS`] (decision
//! 0016, amended 2026-10-01):
//!
//! - **bootstrap** -- read before the database is open, from the bootstrap file, `HS__`
//!   variables or the command line only, never stored in the database (decision 0010; the same
//!   pointers as [`crate::bootstrap::BOOTSTRAP_SETTINGS`]). The admin API refuses a write to one.
//! - **hot** -- something in the running process re-reads it, so a change applies at once
//!   (`hs_cli::live_config` is where each one is wired). A save reports its section as
//!   `reloaded`.
//! - **restart** -- read once, at startup, into something that cannot be re-pointed safely
//!   while it runs. A save stores it and reports its section as `requires_restart`.
//!
//! This is a statement about what `hs serve` actually does, not about what would be possible: a
//! setting is hot only when the change that makes it hot also wires something to re-read it.
//! The schema-walk test below fails the moment a setting is added without a classification.
//!
//! # Read by something, every one
//!
//! Since 2026-10-08 every setting has a reader. The ones decision 0016's amendment found read
//! by nothing either got one (`server.admin_contact` is `/.well-known/matrix/support`,
//! `auth.password.enabled` is read by `GET` and `POST /login`, `media.remote_media_retention`
//! by the remote-media sweeper) or left the schema because nothing should read them
//! (`server.report_stats`, `auth.enable_legacy_login`, `auth.session_secret(_file)`,
//! `appservices.enabled`; [`crate::retired`] drops them from older configurations with a
//! warning).
//!
//! # What stays restart, and why
//!
//! - `media.storage`, `media.scanning` -- an open object store and a scan engine with its
//!   verdict cache and provider connections; in-flight uploads hold the old ones.
//! - `media.allow_legacy_unauthenticated_media` -- the legacy routes are mounted or not.
//! - `federation.enabled`, the TLS trust settings, `client_timeout`, `max_retry_backoff`,
//!   `max_queued_pdus_per_destination` -- built into the federation client and sender, whose
//!   connection pools and queues are in use.
//! - `auth.oidc_providers`, `auth.mas_delegation` -- upstream clients and the issuer depend on
//!   them; changing them under a running server would invalidate sessions on an uncontrolled
//!   boundary.
//! - `telemetry` other than the log level -- the subscriber, exporters and Sentry client are
//!   installed once per process.
//! - `cluster.room_shards`, `user_shards` (fixed at cluster creation), `heartbeat_interval`,
//!   `lease_ttl` (agreed with every replica; a coordinated rolling restart changes them).

use std::sync::LazyLock;

use serde_json::Value;

use crate::Config;

/// When a change to a setting takes effect. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applies {
    /// Per process, from the bootstrap file and environment only; never stored in the database.
    Bootstrap,
    /// Applied by the running server as soon as it changes.
    Hot,
    /// Stored at once, read at the next start.
    Restart,
}

impl Applies {
    /// The name the schema's `x-applies` and the admin API use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Applies::Bootstrap => "bootstrap",
            Applies::Hot => "hot",
            Applies::Restart => "restart",
        }
    }
}

/// One classified setting: a JSON Pointer into the whole configuration (everything beneath it
/// shares its classification), when a change to it applies, and what reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    /// The setting (`/rate_limits/login`). No entry lies beneath another.
    pub pointer: &'static str,
    /// When a change applies.
    pub applies: Applies,
    /// What reads it, in a few words: why it is hot, or why it waits for a restart.
    pub reader: &'static str,
}

const fn hot(pointer: &'static str, reader: &'static str) -> Setting {
    Setting {
        pointer,
        applies: Applies::Hot,
        reader,
    }
}

const fn restart(pointer: &'static str, reader: &'static str) -> Setting {
    Setting {
        pointer,
        applies: Applies::Restart,
        reader,
    }
}

const fn bootstrap(pointer: &'static str, reader: &'static str) -> Setting {
    Setting {
        pointer,
        applies: Applies::Bootstrap,
        reader,
    }
}

/// Every setting's classification. Every setting the schema declares lies at or beneath exactly
/// one entry (the tests walk the schema to make sure), and no entry lies beneath another.
pub const SETTINGS: &[Setting] = &[
    // server
    bootstrap(
        "/server/server_name",
        "fixed at the first start and recorded as the database's identity",
    ),
    bootstrap(
        "/server/signing_key_path",
        "a path on this process's own filesystem, read at startup",
    ),
    hot(
        "/server/public_baseurl",
        "the client .well-known document, the recovery link and bridge files read it per use",
    ),
    hot(
        "/server/well_known_server",
        "the server .well-known document reads it per request",
    ),
    hot(
        "/server/admin_contact",
        "GET /.well-known/matrix/support reads it per request",
    ),
    hot(
        "/server/unstable_features",
        "GET /versions reads it per request",
    ),
    hot(
        "/server/sync",
        "the session hub reads it on every room update",
    ),
    // listeners, storage
    bootstrap("/listeners", "sockets this process binds at startup"),
    bootstrap("/storage", "where the database is, read before it is open"),
    // media
    restart(
        "/media/storage",
        "the object store is opened once; in-flight uploads hold it",
    ),
    hot(
        "/media/max_upload_size",
        "the media repository checks every upload and remote fetch against it",
    ),
    hot(
        "/media/thumbnail_sizes",
        "the media repository reads it per thumbnail request",
    ),
    hot(
        "/media/max_image_pixels",
        "the media repository reads it per thumbnail it makes",
    ),
    hot(
        "/media/max_image_dimension",
        "the media repository reads it per thumbnail it makes",
    ),
    hot(
        "/media/max_image_decode_memory",
        "the media repository reads it per thumbnail it makes",
    ),
    hot(
        "/media/url_preview_enabled",
        "the media repository reads it per preview request",
    ),
    hot(
        "/media/url_preview_ip_range_blocklist",
        "the media repository reads it per preview and remote fetch",
    ),
    hot(
        "/media/remote_media_retention",
        "the remote-media sweeper reads it on every pass",
    ),
    restart(
        "/media/allow_legacy_unauthenticated_media",
        "the legacy media routes are mounted or not at startup",
    ),
    hot(
        "/media/url_preview_timeout",
        "the media repository reads it per preview request",
    ),
    hot(
        "/media/url_preview_max_fetch_size",
        "the media repository reads it per preview request",
    ),
    hot(
        "/media/url_preview_cache_lifetime",
        "the media repository reads it per preview request",
    ),
    restart(
        "/media/scanning",
        "the scan engine, its provider connections and verdict cache are built once",
    ),
    // email: every setting is read per email sent (hs_push::email), so each is hot.
    hot("/email/smtp", "the mailer connects per email sent"),
    hot("/email/from", "the mailer reads it per email sent"),
    hot("/email/app_name", "the mailer reads it per email sent"),
    hot(
        "/email/client_base_url",
        "the mailer reads it per email sent",
    ),
    hot(
        "/email/notifications",
        "the email pusher worker reads it per notification",
    ),
    // network
    hot(
        "/network/outbound/ipv4_only",
        "every outbound client's resolver reads it per new connection (hs_http::outbound)",
    ),
    // federation
    restart(
        "/federation/enabled",
        "the federation routes and client are mounted or not at startup",
    ),
    hot(
        "/federation/domain_allowlist",
        "the federation client checks it on every request",
    ),
    hot(
        "/federation/ip_range_blocklist",
        "the federation client checks it on every request",
    ),
    hot(
        "/federation/ip_range_allowlist",
        "the federation client checks it on every request",
    ),
    restart(
        "/federation/verify_certificates",
        "built into the federation client's TLS configuration",
    ),
    restart(
        "/federation/custom_ca_certificates",
        "built into the federation client's TLS configuration",
    ),
    restart(
        "/federation/trust_os_root_store",
        "built into the federation client's TLS configuration",
    ),
    restart(
        "/federation/client_timeout",
        "built into the federation client",
    ),
    restart(
        "/federation/max_retry_backoff",
        "built into the federation client's backoff",
    ),
    hot(
        "/federation/key_fetch_timeout",
        "the key cache reads it for each fetch of another server's keys",
    ),
    restart(
        "/federation/max_queued_pdus_per_destination",
        "built into the federation sender's queues",
    ),
    hot(
        "/federation/max_queued_durable_edus_per_destination",
        "the federation sender reads it for each update it queues",
    ),
    hot(
        "/federation/forget_unused_destinations_after",
        "the destination sweep reads it each time it runs (hourly)",
    ),
    hot(
        "/federation/allow_public_rooms_over_federation",
        "the federation routes read it per request",
    ),
    hot(
        "/federation/allow_device_name_lookup_over_federation",
        "the federation routes read it per request",
    ),
    hot(
        "/federation/trusted_key_servers",
        "the key cache reads it each time it asks a notary for a key a server does not publish",
    ),
    // rate_limits
    hot(
        "/rate_limits/enabled",
        "every rate-limit bucket reads it on its next check",
    ),
    hot(
        "/rate_limits/message",
        "the room layer's send limiter, on sending, state and redaction",
    ),
    hot(
        "/rate_limits/registration",
        "POST /register, per client address",
    ),
    hot("/rate_limits/login", "POST /login, per client address"),
    hot(
        "/rate_limits/joins_local",
        "joins to rooms this server hosts, per user",
    ),
    hot(
        "/rate_limits/joins_remote",
        "joins through another server, per user",
    ),
    hot(
        "/rate_limits/admin_redaction",
        "redactions by server administrators, per user",
    ),
    hot(
        "/rate_limits/federation",
        "inbound federation transactions, per origin server",
    ),
    hot(
        "/rate_limits/third_party_id_validation",
        "validation emails requested, per client address and per email address",
    ),
    // auth
    hot(
        "/auth/enable_registration",
        "POST /register reads it per request",
    ),
    hot(
        "/auth/allow_guest_access",
        "POST /register?kind=guest reads it per request",
    ),
    hot(
        "/auth/identity_servers",
        "a third-party invite reads it per request",
    ),
    hot(
        "/auth/registration_shared_secret",
        "shared-secret registration and login read it per request",
    ),
    hot(
        "/auth/registration_shared_secret_file",
        "shared-secret registration and login read it per request",
    ),
    hot(
        "/auth/user_directory_search_all_users",
        "the user directory reads it per search",
    ),
    hot("/auth/access_token_lifetime", "read when a token is issued"),
    hot(
        "/auth/refresh_token_lifetime",
        "read when a token is issued",
    ),
    hot(
        "/auth/password/enabled",
        "GET and POST /login read it per request",
    ),
    hot("/auth/password/pepper", "read when a password is checked"),
    hot(
        "/auth/password/pepper_file",
        "read when a password is checked",
    ),
    hot("/auth/password/policy", "read when a password is set"),
    hot("/auth/recaptcha", "POST /register reads it per request"),
    restart(
        "/auth/oidc_providers",
        "upstream OIDC clients are built at startup",
    ),
    hot("/auth/cas", "CAS sign-in reads it per request"),
    hot("/auth/sso", "single sign-on reads it per sign-in"),
    hot(
        "/auth/next_link_domain_whitelist",
        "a validation email request reads it",
    ),
    restart(
        "/auth/mas_delegation",
        "delegation replaces the native issuer at startup",
    ),
    // appservices
    bootstrap(
        "/appservices/registration_files",
        "imported once into the registry at startup",
    ),
    hot(
        "/appservices/tracking_failure_threshold",
        "the appservice registry reads it per health check",
    ),
    // telemetry
    restart(
        "/telemetry/metrics",
        "the metrics registry and exporter are installed once",
    ),
    restart(
        "/telemetry/tracing",
        "the tracing exporter is installed once",
    ),
    hot(
        "/telemetry/logging/level",
        "the log filter sits behind a reload layer (unless RUST_LOG set it)",
    ),
    restart(
        "/telemetry/logging/json",
        "the log format is installed once",
    ),
    restart("/telemetry/sentry", "the Sentry client is installed once"),
    // cluster
    bootstrap(
        "/cluster/single_node",
        "this replica's own role, read at startup",
    ),
    restart("/cluster/room_shards", "fixed at cluster creation"),
    restart("/cluster/user_shards", "fixed at cluster creation"),
    bootstrap("/cluster/mesh", "this replica's own mesh identity"),
    restart(
        "/cluster/heartbeat_interval",
        "agreed with every replica; changed by a rolling restart",
    ),
    restart(
        "/cluster/lease_ttl",
        "agreed with every replica; changed by a rolling restart",
    ),
    // migration
    hot(
        "/migration/synapse",
        "read when a migration starts, never at startup",
    ),
];

/// True when `pointer` is `ancestor` or lies beneath it.
fn is_within(pointer: &str, ancestor: &str) -> bool {
    pointer == ancestor
        || pointer
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The classified setting `pointer` (a whole-configuration JSON Pointer) is, or lies beneath.
/// `None` for a section or structure whose settings are classified one by one
/// (`/telemetry/logging`), and for anything that is not a setting.
#[must_use]
pub fn setting(pointer: &str) -> Option<&'static Setting> {
    SETTINGS
        .iter()
        .find(|setting| is_within(pointer, setting.pointer))
}

/// When a change to the setting at `pointer` takes effect. A structure whose settings are
/// classified one by one counts as hot only when every one of them is, and as bootstrap only
/// when every one is; anything unclassified counts as restart, which is never wrong about a
/// running server.
#[must_use]
pub fn applies(pointer: &str) -> Applies {
    if let Some(setting) = setting(pointer) {
        return setting.applies;
    }
    let mut beneath = SETTINGS
        .iter()
        .filter(|setting| is_within(setting.pointer, pointer))
        .map(|setting| setting.applies)
        .peekable();
    let Some(first) = beneath.peek().copied() else {
        return Applies::Restart;
    };
    if beneath.all(|applies| applies == first) {
        first
    } else {
        Applies::Restart
    }
}

/// The settings a running server re-reads when they change, as JSON Pointers into the whole
/// configuration: every [`Applies::Hot`] entry of [`SETTINGS`]. A pointer covers everything
/// beneath it.
pub static HOT_SETTINGS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    SETTINGS
        .iter()
        .filter(|setting| setting.applies == Applies::Hot)
        .map(|setting| setting.pointer)
        .collect()
});

/// Top-level [`Config`] field names in which every administered setting is hot: a change
/// anywhere in them that the admin API accepts takes effect without a restart. A section with
/// only some hot settings is not listed; [`is_hot_setting`] answers for those one setting at a
/// time.
pub static RELOADABLE_SECTIONS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    SECTION_NAMES
        .iter()
        .copied()
        .filter(|section| {
            let prefix = format!("/{section}");
            let mut administered = SETTINGS
                .iter()
                .filter(|setting| is_within(setting.pointer, &prefix))
                .filter(|setting| setting.applies != Applies::Bootstrap)
                .peekable();
            administered.peek().is_some()
                && administered.all(|setting| setting.applies == Applies::Hot)
        })
        .collect()
});

/// True when every administered setting in `section` (a top-level `Config` field name) takes
/// effect without a restart.
pub fn is_reloadable(section: &str) -> bool {
    RELOADABLE_SECTIONS.contains(&section)
}

/// True when the setting at `pointer` (a JSON Pointer into the whole configuration, like
/// `/rate_limits/message/burst_count`) takes effect without a restart: it is, or is beneath,
/// one of [`HOT_SETTINGS`].
pub fn is_hot_setting(pointer: &str) -> bool {
    setting(pointer).is_some_and(|setting| setting.applies == Applies::Hot)
}

/// The hot settings (entries of [`HOT_SETTINGS`]) whose value differs between `old` and `new`,
/// as whole configurations' JSON. What a running server on `old` applies, setting by setting.
#[must_use]
pub fn hot_settings_changed(old: &Value, new: &Value) -> Vec<&'static str> {
    HOT_SETTINGS
        .iter()
        .copied()
        .filter(|pointer| old.pointer(pointer) != new.pointer(pointer))
        .collect()
}

/// `config` as a JSON object, or `None` if it does not serialize (a `Config` always does).
fn as_object(config: &Config) -> Option<Value> {
    serde_json::to_value(config).ok().filter(Value::is_object)
}

/// `whole` with every hot setting taken out, so that what is left compares equal exactly when
/// nothing that needs a restart changed.
fn without_hot(mut whole: Value) -> Value {
    for pointer in HOT_SETTINGS.iter() {
        let Some((parent, key)) = pointer.rsplit_once('/') else {
            continue;
        };
        let parent = if parent.is_empty() {
            Some(&mut whole)
        } else {
            whole.pointer_mut(parent)
        };
        if let Some(Value::Object(map)) = parent {
            map.remove(key);
        }
    }
    whole
}

/// Compares `old` and `new` and returns the names of the sections in which a setting that is
/// **not** hot changed — the set an operator must restart the process for. An empty result
/// means a running server on `old` can take on `new` in full. Fails safe: a configuration that
/// cannot be compared counts every section as needing a restart.
pub fn sections_requiring_restart(old: &Config, new: &Config) -> Vec<&'static str> {
    let (Some(old), Some(new)) = (as_object(old), as_object(new)) else {
        return SECTION_NAMES.to_vec();
    };
    let (old, new) = (without_hot(old), without_hot(new));
    SECTION_NAMES
        .iter()
        .copied()
        .filter(|name| old.get(*name) != new.get(*name))
        .collect()
}

/// The names of the sections in which a hot setting differs between `old` and `new`: what a
/// running server on `old` re-reads to be on `new`. A section can be both here and in
/// [`sections_requiring_restart`] when settings of both kinds in it changed.
pub fn hot_sections_changed(old: &Config, new: &Config) -> Vec<&'static str> {
    let (Some(old), Some(new)) = (as_object(old), as_object(new)) else {
        return Vec::new();
    };
    let mut out: Vec<&'static str> = Vec::new();
    for pointer in hot_settings_changed(&old, &new) {
        if let Some(section) = section_name(pointer)
            && !out.contains(&section)
        {
            out.push(section);
        }
    }
    out
}

/// The top-level section `pointer` lies in, as one of [`SECTION_NAMES`].
#[must_use]
pub fn section_name(pointer: &str) -> Option<&'static str> {
    let section = crate::document::section_of(pointer)?;
    SECTION_NAMES.iter().copied().find(|name| *name == section)
}

/// Every top-level `Config` field name, reloadable or not. Kept in sync
/// with the `Config` struct by the test below.
pub const SECTION_NAMES: &[&str] = &[
    "server",
    "listeners",
    "storage",
    "media",
    "federation",
    "rate_limits",
    "auth",
    "appservices",
    "telemetry",
    "cluster",
    "migration",
    "network",
    "email",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_names_match_config_schema() {
        let default = Config::default();
        let v = serde_json::to_value(&default).unwrap();
        let obj = v.as_object().unwrap();
        let mut schema_keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        schema_keys.sort_unstable();
        let mut known: Vec<&str> = SECTION_NAMES.to_vec();
        known.sort_unstable();
        assert_eq!(
            schema_keys, known,
            "SECTION_NAMES drifted from the Config struct"
        );
    }

    /// The test the classification exists for: walk every setting the schema declares and
    /// require each to be classified exactly once -- at or beneath one entry of [`SETTINGS`], or
    /// a structure whose every setting is. A setting added to the schema without a line in
    /// [`SETTINGS`] fails here, naming itself.
    #[test]
    fn every_setting_in_the_schema_is_classified_exactly_once() {
        let fields = crate::schema::field_pointers();
        let mut unclassified = Vec::new();
        for field in &fields {
            let covering: Vec<&Setting> = SETTINGS
                .iter()
                .filter(|setting| is_within(field, setting.pointer))
                .collect();
            match covering.len() {
                1 => {}
                0 => {
                    // A structure split into classified settings is fine; its settings are
                    // each checked in their own right.
                    let split = SETTINGS
                        .iter()
                        .any(|setting| is_within(setting.pointer, field));
                    if !split {
                        unclassified.push(field.clone());
                    }
                }
                _ => panic!(
                    "{field} is classified more than once: {:?}",
                    covering.iter().map(|s| s.pointer).collect::<Vec<_>>()
                ),
            }
        }
        assert!(
            unclassified.is_empty(),
            "settings with no classification in hs_config::reload::SETTINGS (add each as \
             bootstrap, hot or restart): {unclassified:#?}"
        );
        // And every entry names a real setting.
        for setting in SETTINGS {
            assert!(
                fields.contains(setting.pointer),
                "{} is not a setting",
                setting.pointer
            );
        }
    }

    #[test]
    fn no_entry_lies_beneath_another() {
        for a in SETTINGS {
            for b in SETTINGS {
                if a.pointer != b.pointer {
                    assert!(
                        !is_within(a.pointer, b.pointer),
                        "{} lies beneath {}",
                        a.pointer,
                        b.pointer
                    );
                }
            }
        }
    }

    #[test]
    fn the_bootstrap_entries_are_the_bootstrap_settings() {
        let mut classified: Vec<&str> = SETTINGS
            .iter()
            .filter(|s| s.applies == Applies::Bootstrap)
            .map(|s| s.pointer)
            .collect();
        let mut bootstrap: Vec<&str> = crate::bootstrap::BOOTSTRAP_SETTINGS
            .iter()
            .map(|s| s.pointer)
            .collect();
        classified.sort_unstable();
        bootstrap.sort_unstable();
        assert_eq!(classified, bootstrap);
    }

    #[test]
    fn every_hot_setting_names_a_real_setting() {
        let whole = serde_json::to_value(Config::default()).unwrap();
        for pointer in HOT_SETTINGS.iter() {
            let section = crate::document::section_of(pointer).unwrap();
            assert!(SECTION_NAMES.contains(&section), "{pointer}");
            // Optional settings serialize as `null` when unset, and still exist.
            assert!(
                whole.pointer(pointer).is_some(),
                "{pointer} is not a setting"
            );
        }
        for section in RELOADABLE_SECTIONS.iter() {
            assert!(
                SETTINGS
                    .iter()
                    .any(|s| s.pointer.starts_with(&format!("/{section}/")))
            );
        }
    }

    #[test]
    fn applies_answers_for_settings_structures_and_unknowns() {
        assert_eq!(applies("/rate_limits/login/per_second"), Applies::Hot);
        assert_eq!(applies("/rate_limits"), Applies::Hot);
        assert_eq!(applies("/storage/data_dir"), Applies::Bootstrap);
        assert_eq!(applies("/listeners"), Applies::Bootstrap);
        assert_eq!(applies("/telemetry/logging/level"), Applies::Hot);
        assert_eq!(applies("/telemetry/logging"), Applies::Restart, "mixed");
        assert_eq!(applies("/media/storage/path"), Applies::Restart);
        assert_eq!(applies("/no/such/setting"), Applies::Restart);
        assert_eq!(
            applies("/auth/oidc_providers/0/client_id"),
            Applies::Restart
        );
    }

    #[test]
    fn the_reloadable_sections_are_those_with_only_hot_administered_settings() {
        let mut sections = RELOADABLE_SECTIONS.clone();
        sections.sort_unstable();
        assert_eq!(
            sections,
            // `appservices` since `appservices.enabled` left the schema (2026-10-08): what is
            // left is the hot failure threshold and the bootstrap registration files.
            vec![
                "appservices",
                "email",
                "migration",
                "network",
                "rate_limits",
                "server"
            ]
        );
    }

    #[test]
    fn a_rate_limit_change_is_hot_and_needs_no_restart() {
        let old = Config::default();
        let mut new = old.clone();
        new.rate_limits.login.per_second = 999.0;
        assert!(sections_requiring_restart(&old, &new).is_empty());
        assert_eq!(hot_sections_changed(&old, &new), vec!["rate_limits"]);
        assert!(is_hot_setting("/rate_limits/message/burst_count"));
        assert!(is_hot_setting("/rate_limits/login"));
        assert!(!is_hot_setting("/rate_limits"), "a section is no setting");
        assert!(!is_hot_setting("/rate_limits_other"));
    }

    #[test]
    fn server_name_change_requires_restart() {
        let mut old = Config::default();
        old.server.server_name = "old.example".into();
        let mut new = old.clone();
        new.server.server_name = "new.example".into();
        assert_eq!(sections_requiring_restart(&old, &new), vec!["server"]);
        assert!(hot_sections_changed(&old, &new).is_empty());
    }

    #[test]
    fn a_setting_nothing_rereads_requires_restart() {
        let old = Config::default();
        let mut new = old.clone();
        new.federation.client_timeout = crate::Duration::from_secs(45);
        assert_eq!(sections_requiring_restart(&old, &new), vec!["federation"]);
        assert!(!is_reloadable("federation"));
    }

    #[test]
    fn a_section_with_some_hot_settings_needs_a_restart_only_for_the_others() {
        let old = Config::default();
        let mut new = old.clone();
        new.federation.domain_allowlist = Some(vec!["friend.example".to_owned()]);
        new.federation.ip_range_blocklist = Vec::new();
        new.federation.allow_public_rooms_over_federation = true;
        assert!(sections_requiring_restart(&old, &new).is_empty());
        assert_eq!(hot_sections_changed(&old, &new), vec!["federation"]);
        assert!(is_hot_setting("/federation/domain_allowlist"));
        assert!(!is_hot_setting("/federation/client_timeout"));

        // And both at once: applied now, and still pending.
        new.federation.client_timeout = crate::Duration::from_secs(45);
        assert_eq!(sections_requiring_restart(&old, &new), vec!["federation"]);
        assert_eq!(hot_sections_changed(&old, &new), vec!["federation"]);
    }

    #[test]
    fn hot_settings_changed_names_each_setting() {
        let old = Config::default();
        let mut new = old.clone();
        new.auth.enable_registration = true;
        new.media.max_upload_size = crate::ByteSize::bytes(10);
        new.media.storage = crate::media::MediaStorageBackend::default();
        let changed = hot_settings_changed(
            &serde_json::to_value(&old).unwrap(),
            &serde_json::to_value(&new).unwrap(),
        );
        assert_eq!(
            changed,
            vec!["/media/max_upload_size", "/auth/enable_registration"]
        );
    }

    #[test]
    fn unchanged_config_needs_no_restart() {
        let old = Config::default();
        let new = old.clone();
        assert!(sections_requiring_restart(&old, &new).is_empty());
        assert!(hot_sections_changed(&old, &new).is_empty());
    }
}
