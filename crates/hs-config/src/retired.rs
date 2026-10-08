//! Settings that were removed from the schema because nothing should read them.
//!
//! A configuration written before a setting was removed may still carry it: a bootstrap file, an
//! `HS__` variable, a document `hs config translate` produced from Synapse's `homeserver.yaml`,
//! or the database layer an earlier version of the admin API wrote. Every section is
//! `deny_unknown_fields`, so without this module such a configuration would stop the server from
//! starting. Instead [`Config::from_value`](crate::Config::from_value) and
//! [`Layers::merged`](crate::Layers::merged) drop each retired setting they find and log it once
//! per load at `warn` (target `hs_config::retired`), naming what replaced it, so an operator can
//! clean it out at leisure.
//!
//! A *new* write of a retired setting is refused: [`Layers::resolve_with_patch`]
//! (crate::Layers::resolve_with_patch), which the admin API validates every change with, answers
//! a validation error naming the setting and why it went.
//!
//! Removed 2026-10-08 (status 16, "settings with no reader"; decision 0016's amendment listed
//! them as read by nothing): see [`RETIRED_SETTINGS`].

use serde_json::Value;

/// One removed setting: where it was (a JSON Pointer into the whole configuration) and what an
/// operator should know instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredSetting {
    /// JSON Pointer, as in [`crate::reload::SETTINGS`].
    pub pointer: &'static str,
    /// Why it went, and what does the job instead, in a sentence an operator can act on.
    pub why: &'static str,
}

/// Every setting removed from the schema.
pub const RETIRED_SETTINGS: &[RetiredSetting] = &[
    RetiredSetting {
        pointer: "/server/report_stats",
        why: "this server never sends usage statistics anywhere, so there is nothing to switch",
    },
    RetiredSetting {
        pointer: "/auth/enable_legacy_login",
        why: "the classic Matrix sign-in is always served beside the OAuth 2.0 issuer; \
              auth.mas_delegation is how to hand sign-in to an external issuer instead",
    },
    RetiredSetting {
        pointer: "/auth/session_secret",
        why: "sessions, tokens and sign-in state are kept in the database, so nothing is signed \
              with a shared secret (Synapse's macaroon_secret_key is not needed either)",
    },
    RetiredSetting {
        pointer: "/auth/session_secret_file",
        why: "sessions, tokens and sign-in state are kept in the database, so nothing is signed \
              with a shared secret (Synapse's macaroon_secret_key is not needed either)",
    },
    RetiredSetting {
        pointer: "/appservices/enabled",
        why: "events are always delivered to registered bridges; pause one bridge on its page \
              in the Bridges section instead",
    },
];

/// The retired setting at `pointer`, if it is one.
#[must_use]
pub fn retired(pointer: &str) -> Option<&'static RetiredSetting> {
    RETIRED_SETTINGS.iter().find(|r| r.pointer == pointer)
}

/// Removes every retired setting from a whole-configuration JSON document, returning the ones it
/// found.
pub fn strip_json(document: &mut Value) -> Vec<&'static RetiredSetting> {
    let mut found = Vec::new();
    for setting in RETIRED_SETTINGS {
        let Some((parent, key)) = setting.pointer.rsplit_once('/') else {
            continue;
        };
        let parent = if parent.is_empty() {
            Some(&mut *document)
        } else {
            document.pointer_mut(parent)
        };
        if let Some(Value::Object(map)) = parent
            && map.remove(key).is_some()
        {
            found.push(setting);
        }
    }
    found
}

/// Removes every retired setting from a parsed YAML document, returning the ones it found.
pub fn strip_yaml(document: &mut serde_yaml_ng::Value) -> Vec<&'static RetiredSetting> {
    fn remove(node: &mut serde_yaml_ng::Value, path: &[&str]) -> bool {
        match path {
            [] => false,
            [key] => node
                .as_mapping_mut()
                .is_some_and(|map| map.remove(*key).is_some()),
            [first, rest @ ..] => node.get_mut(*first).is_some_and(|next| remove(next, rest)),
        }
    }
    RETIRED_SETTINGS
        .iter()
        .filter(|setting| {
            let path: Vec<&str> = setting.pointer.split('/').skip(1).collect();
            remove(document, &path)
        })
        .collect()
}

/// Logs what [`strip_json`] or [`strip_yaml`] found, once per setting per process: the merged
/// configuration is recomputed on every read, and one line is enough for an operator to act on.
/// The configuration is first read before `hs serve` has installed its log subscriber; a
/// setting found then is logged the next time it is found, once something listens.
pub fn warn_dropped(found: &[&'static RetiredSetting], from: &str) {
    static WARNED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    if found.is_empty() || !tracing::dispatcher::has_been_set() {
        return;
    }
    for setting in found {
        {
            let mut warned = WARNED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if warned.contains(&setting.pointer) {
                continue;
            }
            warned.push(setting.pointer);
        }
        tracing::warn!(
            target: "hs_config::retired",
            setting = setting.pointer,
            from,
            "ignoring a setting that no longer exists: {}; remove it from the configuration",
            setting.why
        );
    }
}

/// The retired settings a patch of `section` would write, as validation messages.
#[must_use]
pub fn in_patch(section: &str, patch: &Value) -> Vec<(String, &'static RetiredSetting)> {
    let mut out = Vec::new();
    for setting in RETIRED_SETTINGS {
        let Some(rest) = setting.pointer.strip_prefix(&format!("/{section}")) else {
            continue;
        };
        if rest.starts_with('/') && patch.pointer(rest).is_some() {
            out.push((setting.pointer[1..].replace('/', "."), setting));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_retired_setting_is_dropped_from_json_and_named() {
        let mut doc = json!({
            "server": {"server_name": "example.org", "report_stats": true},
            "auth": {"enable_registration": false, "session_secret": "s"},
        });
        let found = strip_json(&mut doc);
        assert_eq!(
            found.iter().map(|r| r.pointer).collect::<Vec<_>>(),
            vec!["/server/report_stats", "/auth/session_secret"]
        );
        assert_eq!(
            doc,
            json!({"server": {"server_name": "example.org"}, "auth": {"enable_registration": false}})
        );
        // Nothing to drop: nothing found, nothing changed.
        assert!(strip_json(&mut doc).is_empty());
    }

    #[test]
    fn a_retired_setting_is_dropped_from_yaml() {
        let mut doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(
            "server:\n  server_name: example.org\nappservices:\n  enabled: false\n",
        )
        .unwrap();
        let found = strip_yaml(&mut doc);
        assert_eq!(found[0].pointer, "/appservices/enabled");
        assert!(doc["appservices"].as_mapping().unwrap().is_empty());
    }

    #[test]
    fn a_patch_naming_a_retired_setting_is_found() {
        let hits = in_patch("auth", &json!({"enable_legacy_login": false}));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "auth.enable_legacy_login");
        assert!(in_patch("auth", &json!({"enable_registration": true})).is_empty());
        assert!(in_patch("server", &json!({"enable_legacy_login": true})).is_empty());
    }

    #[test]
    fn no_retired_setting_is_still_in_the_schema() {
        let pointers = crate::schema::field_pointers();
        for setting in RETIRED_SETTINGS {
            assert!(
                !pointers.iter().any(|p| p == setting.pointer),
                "{} is retired but still in the schema",
                setting.pointer
            );
        }
    }
}
