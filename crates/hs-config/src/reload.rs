//! The reload boundary: which top-level [`Config`](crate::Config) sections
//! may be swapped into a running server without a restart.
//!
//! # Reloadable
//!
//! - `rate_limits` — pure policy, re-read by the rate limiter on the next
//!   request.
//! - `federation` — allow/deny lists and timeouts read per outbound or
//!   inbound request; nothing is bound to their current value at startup.
//! - `telemetry` — log level, trace sampling and metrics toggles are read
//!   by the logging/tracing layer on each event.
//! - `appservices` — the registry is explicitly designed for hot
//!   registration (`PLAN.md` D7); this section only lists static
//!   registration files, re-scanned on SIGHUP or an admin-API reload call.
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

use crate::Config;

/// Top-level [`Config`] field names that may change on a running server
/// without a restart. Order matches [`Config`]'s field declaration order
/// but that is not load-bearing; this is a set.
pub const RELOADABLE_SECTIONS: &[&str] = &["rate_limits", "federation", "telemetry", "appservices"];

/// True when `section` (a top-level `Config` field name) is in the
/// reloadable set.
pub fn is_reloadable(section: &str) -> bool {
    RELOADABLE_SECTIONS.contains(&section)
}

/// Compares every top-level section of `old` and `new` and returns the
/// names of sections that changed and are **not** reloadable — the set an
/// operator must restart the process for. An empty result means `new` can
/// be hot-applied in full (reloadable sections that changed are applied;
/// unchanged non-reloadable sections are, by definition, not a problem).
pub fn sections_requiring_restart(old: &Config, new: &Config) -> Vec<&'static str> {
    let old_v = serde_json::to_value(old).expect("Config always serializes");
    let new_v = serde_json::to_value(new).expect("Config always serializes");
    let (Some(old_obj), Some(new_obj)) = (old_v.as_object(), new_v.as_object()) else {
        // Should not happen for a struct, but fail safe: if we cannot
        // compare structurally, assume everything needs a restart.
        return SECTION_NAMES.to_vec();
    };
    let mut out = Vec::new();
    for &name in SECTION_NAMES {
        if is_reloadable(name) {
            continue;
        }
        if old_obj.get(name) != new_obj.get(name) {
            out.push(name);
        }
    }
    out
}

/// Every top-level `Config` field name, reloadable or not. Kept in sync
/// with the `Config` struct by the test below.
const SECTION_NAMES: &[&str] = &[
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
    fn reloadable_change_requires_no_restart() {
        let old = Config::default();
        let mut new = old.clone();
        new.rate_limits.login.per_second = 999.0;
        assert!(sections_requiring_restart(&old, &new).is_empty());
    }

    #[test]
    fn server_name_change_requires_restart() {
        let mut old = Config::default();
        old.server.server_name = "old.example".into();
        let mut new = old.clone();
        new.server.server_name = "new.example".into();
        assert_eq!(sections_requiring_restart(&old, &new), vec!["server"]);
    }

    #[test]
    fn unchanged_config_needs_no_restart() {
        let old = Config::default();
        let new = old.clone();
        assert!(sections_requiring_restart(&old, &new).is_empty());
    }
}
