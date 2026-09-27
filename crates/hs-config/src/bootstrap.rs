//! The bootstrap settings: what a process must know before it can read anything from its
//! database, and which therefore never live in it (decision 0010).
//!
//! Everything else in [`Config`](crate::Config) is administered: stored in the database
//! ([`crate::store`]), edited through the admin API's `/config` operations and the web interface,
//! and shared by every replica that opens the same database. The settings listed here are the
//! exception, and each is an exception for one of six reasons ([`BootstrapReason`]):
//!
//! - **Where the database is** (`storage`). Read before the database is open; it cannot be
//!   stored in the thing it points at.
//! - **What this process listens on** (`listeners`). A socket is bound by one process on one
//!   host, and two replicas may legitimately differ.
//! - **This replica's place in the cluster** (`cluster.single_node`, the whole of
//!   `cluster.mesh`). Per replica by definition: each pod has its own advertised address, and the
//!   mesh's port and certificate paths are what that pod's manifest mounts.
//! - **A path on this process's filesystem** (`server.signing_key_path`). Meaningful only where
//!   the volume is mounted.
//! - **The server's name** (`server.server_name`). Declared once, on the first run, from the
//!   command line, the environment or the bootstrap file. It is not a setting at all after that:
//!   it is burned into every identifier the server has issued, so the store records it as the
//!   database's identity ([`crate::store::ConfigMeta::server_name`]) rather than as an editable
//!   value.
//! - **A one-time import source** (`appservices.registration_files`). Registration files are
//!   read once into the appservice registry on the first start that sees them; after that the
//!   registry, edited through the Bridges section, is the truth.
//!
//! # Why they are never seeded
//!
//! The store used to be seeded with the whole bootstrap file on the first start. In cluster mode
//! that was a bug with a sharp edge: two replicas booting on one PostgreSQL each seeded their own
//! `listeners` and `cluster.mesh.port`, one of them won, and on the next restart the database
//! (which outranks the file) handed the winner's ports to both, so the loser failed to bind.
//! Keeping these out of the database entirely -- not seeded, not writable, and ignored if an
//! older version left them there -- makes each replica's own file and environment the only
//! source for them.
//!
//! [`Layers`](crate::Layers) strips them from the database layer when it resolves, so a store
//! written by an earlier version cannot override them either; [`crate::store::ConfigStore::
//! purge_bootstrap`] removes them from such a store for good.

use serde_json::{Map, Value};

use crate::document::leaf_pointers;

/// Why a setting is bootstrap rather than administered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapReason {
    /// It says where the database is.
    Database,
    /// It is a socket this process binds.
    Listener,
    /// It is this replica's own identity or role in the cluster.
    Replica,
    /// It is a path on this process's own filesystem.
    LocalPath,
    /// It is the server's name, fixed at the first run.
    ServerName,
    /// It names files imported once into a registry, not a live setting.
    Import,
}

impl BootstrapReason {
    /// A sentence an operator can read, for error messages and the admin API.
    #[must_use]
    pub fn explanation(self) -> &'static str {
        match self {
            BootstrapReason::Database => {
                "says where this server's database is, so it is read before the database is open"
            }
            BootstrapReason::Listener => {
                "is a socket this process binds, and each replica binds its own"
            }
            BootstrapReason::Replica => {
                "is this replica's own identity in the cluster, and each replica has its own"
            }
            BootstrapReason::LocalPath => "is a path on this process's own filesystem",
            BootstrapReason::ServerName => {
                "is the server's name, fixed at the first start and recorded as the database's identity"
            }
            BootstrapReason::Import => {
                "lists files imported once at startup; after that the registry, edited in the \
                 Bridges section, is the truth"
            }
        }
    }
}

/// One bootstrap setting: a JSON Pointer into the whole configuration (everything underneath it
/// is bootstrap too) and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapSetting {
    /// The setting, as a JSON Pointer (`/cluster/mesh`). A section-level pointer (`/storage`)
    /// makes the whole section bootstrap.
    pub pointer: &'static str,
    /// Why it cannot be administered through the database.
    pub reason: BootstrapReason,
}

/// Every bootstrap setting. Everything not under one of these pointers is administered.
pub const BOOTSTRAP_SETTINGS: &[BootstrapSetting] = &[
    BootstrapSetting {
        pointer: "/storage",
        reason: BootstrapReason::Database,
    },
    BootstrapSetting {
        pointer: "/listeners",
        reason: BootstrapReason::Listener,
    },
    BootstrapSetting {
        pointer: "/server/server_name",
        reason: BootstrapReason::ServerName,
    },
    BootstrapSetting {
        pointer: "/server/signing_key_path",
        reason: BootstrapReason::LocalPath,
    },
    BootstrapSetting {
        pointer: "/cluster/single_node",
        reason: BootstrapReason::Replica,
    },
    BootstrapSetting {
        pointer: "/cluster/mesh",
        reason: BootstrapReason::Replica,
    },
    BootstrapSetting {
        pointer: "/appservices/registration_files",
        reason: BootstrapReason::Import,
    },
];

/// True when `pointer` is `ancestor` or lies underneath it.
fn is_within(pointer: &str, ancestor: &str) -> bool {
    pointer == ancestor
        || pointer
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The bootstrap setting `pointer` (a whole-configuration JSON Pointer) falls under, if any.
#[must_use]
pub fn bootstrap_setting(pointer: &str) -> Option<&'static BootstrapSetting> {
    BOOTSTRAP_SETTINGS
        .iter()
        .find(|setting| is_within(pointer, setting.pointer))
}

/// True when `pointer` names a bootstrap setting or something inside one.
#[must_use]
pub fn is_bootstrap_pointer(pointer: &str) -> bool {
    bootstrap_setting(pointer).is_some()
}

/// True when the whole of `section` is bootstrap (`storage`, `listeners`).
#[must_use]
pub fn is_bootstrap_section(section: &str) -> bool {
    let pointer = format!("/{section}");
    BOOTSTRAP_SETTINGS
        .iter()
        .any(|setting| setting.pointer == pointer)
}

/// The whole-configuration pointers a merge patch against `section` would write that are
/// bootstrap. Empty means the patch touches administered settings only.
///
/// A leaf that is an *ancestor* of a bootstrap setting counts too: `{"mesh": null}` against
/// `cluster` would remove the mesh settings, which is as much a write to them as setting one.
#[must_use]
pub fn bootstrap_pointers_in_patch(section: &str, patch: &Value) -> Vec<String> {
    let prefix = format!("/{section}");
    leaf_pointers(patch)
        .into_iter()
        .map(|leaf| format!("{prefix}{leaf}"))
        .filter(|pointer| {
            BOOTSTRAP_SETTINGS.iter().any(|setting| {
                is_within(pointer, setting.pointer) || is_within(setting.pointer, pointer)
            })
        })
        .collect()
}

/// Removes every bootstrap setting from a whole-configuration `document`, in place, and returns
/// the pointers it removed. A section left empty by the removal is removed too, so "the database
/// sets nothing here" stays one state rather than two.
pub fn strip_bootstrap(document: &mut Value) -> Vec<String> {
    let mut removed = Vec::new();
    for setting in BOOTSTRAP_SETTINGS {
        if remove_pointer(document, setting.pointer) {
            removed.push(setting.pointer.to_owned());
        }
    }
    if let Some(sections) = document.as_object_mut() {
        sections.retain(|_, value| !value.as_object().is_some_and(Map::is_empty));
    }
    removed
}

/// A copy of `document` without its bootstrap settings.
#[must_use]
pub fn without_bootstrap(document: &Value) -> Value {
    let mut copy = document.clone();
    strip_bootstrap(&mut copy);
    copy
}

/// Removes the value at `pointer` from `document`. Returns whether there was one.
fn remove_pointer(document: &mut Value, pointer: &str) -> bool {
    let Some((parent, last)) = pointer.rsplit_once('/') else {
        return false;
    };
    let parent = if parent.is_empty() {
        Some(document)
    } else {
        document.pointer_mut(parent)
    };
    parent
        .and_then(Value::as_object_mut)
        .is_some_and(|map| map.remove(last).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_pointer_under_a_bootstrap_setting_is_bootstrap() {
        assert!(is_bootstrap_pointer("/storage"));
        assert!(is_bootstrap_pointer("/storage/data_dir"));
        assert!(is_bootstrap_pointer("/cluster/mesh/port"));
        assert!(is_bootstrap_pointer("/cluster/mesh/tls/certificate_path"));
        assert!(is_bootstrap_pointer("/server/server_name"));
        assert!(!is_bootstrap_pointer("/server/public_baseurl"));
        assert!(!is_bootstrap_pointer("/cluster/lease_ttl"));
        assert!(
            !is_bootstrap_pointer("/cluster/meshy"),
            "a sibling that shares a prefix is not inside it"
        );
    }

    #[test]
    fn only_storage_and_listeners_are_whole_bootstrap_sections() {
        let whole: Vec<&str> = crate::reload::SECTION_NAMES
            .iter()
            .copied()
            .filter(|s| is_bootstrap_section(s))
            .collect();
        assert_eq!(whole, vec!["listeners", "storage"]);
    }

    #[test]
    fn every_bootstrap_pointer_names_a_real_setting() {
        let defaults = serde_json::to_value(crate::Config::default()).unwrap();
        for setting in BOOTSTRAP_SETTINGS {
            assert!(
                defaults.pointer(setting.pointer).is_some(),
                "{} is not in the schema",
                setting.pointer
            );
        }
    }

    #[test]
    fn a_patch_is_checked_leaf_by_leaf_and_ancestors_count() {
        assert_eq!(
            bootstrap_pointers_in_patch("cluster", &json!({"mesh": {"port": 9000}})),
            vec!["/cluster/mesh/port"]
        );
        assert_eq!(
            bootstrap_pointers_in_patch("cluster", &json!({"mesh": null})),
            vec!["/cluster/mesh"]
        );
        assert!(bootstrap_pointers_in_patch("cluster", &json!({"lease_ttl": "20s"})).is_empty());
        assert_eq!(
            bootstrap_pointers_in_patch(
                "server",
                &json!({"server_name": "x.example", "admin_contact": "a@b"})
            ),
            vec!["/server/server_name"]
        );
    }

    #[test]
    fn stripping_leaves_administered_settings_and_drops_emptied_sections() {
        let mut document = json!({
            "server": {"server_name": "example.org", "public_baseurl": "https://m.example.org"},
            "listeners": {"listeners": [{"port": 8008}]},
            "cluster": {"mesh": {"port": 8449}},
            "auth": {"enable_registration": true},
        });
        let mut removed = strip_bootstrap(&mut document);
        removed.sort();
        assert_eq!(
            removed,
            vec!["/cluster/mesh", "/listeners", "/server/server_name"]
        );
        assert_eq!(
            document,
            json!({
                "server": {"public_baseurl": "https://m.example.org"},
                "auth": {"enable_registration": true},
            })
        );
    }
}
