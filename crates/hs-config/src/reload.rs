//! The reload boundary: which settings a running server takes on when they change, and which
//! wait for a restart.
//!
//! This is a statement about what `hs serve` actually does, not about what would be possible:
//! a setting is listed in [`HOT_SETTINGS`] only when something in the running process re-reads
//! it after a change (`hs_cli::live_config` is where each one is wired). Everything else is read
//! once at startup, and a change to it is reported as needing a restart.
//!
//! # Hot
//!
//! - `rate_limits` — the whole section. The server-wide `message` limit (the bucket this server
//!   enforces, on sending, state and redaction) is swapped into the room layer's limiter the
//!   moment it changes; senders keep what is left of their bucket, clamped to the new burst.
//!   The other buckets are not enforced anywhere yet, so a change to them has nothing to wait
//!   for either.
//! - `migration` — read when a migration from Synapse starts, never at startup.
//! - `federation.domain_allowlist`, `federation.ip_range_blocklist` and
//!   `federation.ip_range_allowlist` — the outbound client checks both lists on every request,
//!   through shared handles the running server replaces. (Only with federation enabled: a server
//!   that booted without it has no client to change.)
//!
//! # Restart required
//!
//! - `server` — `server_name` is burned into every event and identifier the
//!   process has already produced; `signing_key_path` is read once into
//!   memory at startup.
//! - `listeners` — sockets are bound at startup; changing ports or TLS
//!   material needs a new bind.
//! - `storage` — the backend holds an open connection pool or an embedded
//!   database handle that is not safely swappable underneath in-flight
//!   transactions.
//! - `media` — the storage backend variant has the same problem as
//!   `storage`; even for `local`, in-flight uploads reference the old path.
//! - `auth` — session-signing secrets are cached in every issued token;
//!   rotating them without a coordinated restart would invalidate sessions
//!   unpredictably rather than on a controlled boundary.
//! - `cluster` — shard counts and mesh identity are agreed with every other
//!   replica; changing them locally without a coordinated rolling restart
//!   would fragment ownership.
//! - The rest of `federation` (enabling it, timeouts, certificates), `telemetry`, and
//!   `appservices` — built into the federation client, the logging layer and the appservice
//!   scheduler once, at startup.

use serde_json::Value;

use crate::Config;

/// The settings a running server re-reads when they change, as JSON Pointers into the whole
/// configuration. A pointer covers everything beneath it: `/rate_limits` is the whole section.
pub const HOT_SETTINGS: &[&str] = &[
    "/rate_limits",
    "/migration",
    "/federation/domain_allowlist",
    "/federation/ip_range_blocklist",
    "/federation/ip_range_allowlist",
];

/// Top-level [`Config`] field names whose every setting is hot (see [`HOT_SETTINGS`]): a change
/// anywhere in them takes effect without a restart. A section with only some hot settings is
/// not listed; [`is_hot_setting`] answers for those one setting at a time.
pub const RELOADABLE_SECTIONS: &[&str] = &["rate_limits", "migration"];

/// True when every setting in `section` (a top-level `Config` field name) takes effect without
/// a restart.
pub fn is_reloadable(section: &str) -> bool {
    RELOADABLE_SECTIONS.contains(&section)
}

/// True when the setting at `pointer` (a JSON Pointer into the whole configuration, like
/// `/rate_limits/message/burst_count`) takes effect without a restart: it is, or is beneath,
/// one of [`HOT_SETTINGS`].
pub fn is_hot_setting(pointer: &str) -> bool {
    HOT_SETTINGS.iter().any(|hot| {
        pointer == *hot
            || pointer
                .strip_prefix(hot)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// `config` as a JSON object, or `None` if it does not serialize (a `Config` always does).
fn as_object(config: &Config) -> Option<Value> {
    serde_json::to_value(config).ok().filter(Value::is_object)
}

/// `whole` with every hot setting taken out, so that what is left compares equal exactly when
/// nothing that needs a restart changed.
fn without_hot(mut whole: Value) -> Value {
    for pointer in HOT_SETTINGS {
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
    for pointer in HOT_SETTINGS {
        if old.pointer(pointer) != new.pointer(pointer)
            && let Some(section) = SECTION_NAMES
                .iter()
                .copied()
                .find(|name| crate::document::section_of(pointer) == Some(*name))
            && !out.contains(&section)
        {
            out.push(section);
        }
    }
    out
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

    #[test]
    fn every_hot_setting_names_a_real_setting() {
        let whole = serde_json::to_value(Config::default()).unwrap();
        for pointer in HOT_SETTINGS {
            let section = crate::document::section_of(pointer).unwrap();
            assert!(SECTION_NAMES.contains(&section), "{pointer}");
            // Optional settings serialize as `null` when unset, and still exist.
            assert!(
                whole.pointer(pointer).is_some(),
                "{pointer} is not a setting"
            );
        }
        for section in RELOADABLE_SECTIONS {
            assert!(HOT_SETTINGS.contains(&format!("/{section}").as_str()));
        }
    }

    #[test]
    fn a_rate_limit_change_is_hot_and_needs_no_restart() {
        let old = Config::default();
        let mut new = old.clone();
        new.rate_limits.login.per_second = 999.0;
        assert!(sections_requiring_restart(&old, &new).is_empty());
        assert_eq!(hot_sections_changed(&old, &new), vec!["rate_limits"]);
        assert!(is_hot_setting("/rate_limits/message/burst_count"));
        assert!(is_hot_setting("/rate_limits"));
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
    fn a_section_nothing_rereads_requires_restart() {
        let old = Config::default();
        let mut new = old.clone();
        new.federation.allow_public_rooms_over_federation =
            !old.federation.allow_public_rooms_over_federation;
        assert_eq!(sections_requiring_restart(&old, &new), vec!["federation"]);
        assert!(!is_reloadable("federation"));
    }

    #[test]
    fn a_section_with_some_hot_settings_needs_a_restart_only_for_the_others() {
        let old = Config::default();
        let mut new = old.clone();
        new.federation.domain_allowlist = Some(vec!["friend.example".to_owned()]);
        new.federation.ip_range_blocklist = Vec::new();
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
    fn unchanged_config_needs_no_restart() {
        let old = Config::default();
        let new = old.clone();
        assert!(sections_requiring_restart(&old, &new).is_empty());
        assert!(hot_sections_changed(&old, &new).is_empty());
    }
}
