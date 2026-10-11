//! Server identity: the homeserver's name and its Ed25519 signing key.

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Validate, ValidationErrors};

/// Server identity. Restart required to change (see [`crate::reload`]):
/// `server_name` is embedded in every user ID, room ID and event this
/// process has ever produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The domain in `@user:server_name`, room aliases and event origins.
    /// Corresponds to Synapse's `server_name`. Changing it after any room
    /// exists is not supported by any Matrix homeserver, including this
    /// one.
    pub server_name: String,

    /// The address clients reach this server at (`https://matrix.example.org`), when it is not
    /// `https://` plus the server name. Clients find it through the `.well-known` document this
    /// server serves when it is set, and links this server hands out use it. When it is an
    /// `https://` URL it also gives other servers their way in: `GET /.well-known/matrix/server`
    /// advertises its host and port unless `well_known_server` says otherwise. Corresponds to
    /// Synapse's `public_baseurl`.
    #[serde(default)]
    pub public_baseurl: Option<String>,

    /// The `host[:port]` other servers connect to for federation, published at
    /// `GET /.well-known/matrix/server`. Unset (the default), it is derived from
    /// `public_baseurl` when that is an `https://` URL: its host, and its port or 443
    /// (`https://matrix.example.org` advertises `matrix.example.org:443`), so a server with a
    /// public address can be found by other servers without further configuration. Set it when
    /// federation is reached at a different host or port than clients use (`matrix.example.org:8448`
    /// while clients use `https://example.org`); set it to the empty string to publish no
    /// document (for example, when a reverse proxy serves one). Nothing is derived from an
    /// `http://` base URL, since federation needs TLS, nor while `federation.enabled` is false.
    /// Corresponds to Synapse's `serve_server_wellknown` plus the document Synapse serves from
    /// it, collapsed into one field (`crates/hs-federation/src/discovery.rs` implements the
    /// resolution order this feeds).
    #[serde(default)]
    pub well_known_server: Option<String>,

    /// Directory holding this server's Ed25519 signing keys. Corresponds to
    /// Synapse's `signing_key_path` (a file here; a directory in our
    /// layout because multiple active keys are normal during rotation).
    #[serde(default = "default_signing_key_path")]
    pub signing_key_path: PathBuf,

    /// How to reach this server's administrator: an email address (`mailto:abuse@example.org`)
    /// or a Matrix ID (`@admin:example.org`). It is published as the administrator contact in
    /// `/.well-known/matrix/support`, the document clients read to tell people who to ask for
    /// help or report abuse to; a `https://` address is published there as the support page
    /// instead. Unset, that document is not served. Corresponds to Synapse's `admin_contact`.
    #[serde(default)]
    pub admin_contact: Option<String>,

    /// Extra `unstable_features` flags advertised by `GET /_matrix/client/versions`, by MSC
    /// identifier (`org.matrix.msc3202: true`). Merged over the server's built-in set, which is
    /// empty: every flag gates a feature a client or bridge will then use, so advertise one only
    /// for a feature this server serves. `false` suppresses a built-in flag.
    #[serde(default)]
    pub unstable_features: BTreeMap<String, bool>,

    /// How much of each user's sync history this server keeps: the per-user feed that tells
    /// `/sync` which rooms changed, and the server-wide stream that does the same for very
    /// large rooms. Older history is compacted to each room's last position. A client whose
    /// sync token is older than what is kept still learns of every room that changed, and
    /// is sent a room whole where its position as of the token is gone; it never misses
    /// anything.
    #[serde(default)]
    pub sync: SyncConfig,

    /// How much of each room this server keeps in memory while the room is in use: the recent
    /// events it holds resident per room, and how long an unused room stays resident. A room's
    /// memory is bounded by its state and this window, not by the length of its history; the
    /// rest is read from the database as needed.
    #[serde(default)]
    pub rooms: RoomsConfig,
}

/// What a room costs in memory (`server.rooms`). Both settings take effect at once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoomsConfig {
    /// How many of a room's events this server keeps in memory, per room, most recently used
    /// first: the ones its members' clients read and its next events cite. An event not kept is
    /// read from the database when a request needs it. A parsed event is a few kilobytes, so
    /// this is at most a few megabytes for each room that has this much history. `0` keeps
    /// none, which costs a database read per event served and gains nothing. Synapse has no
    /// equivalent: it caches events across rooms (`event_cache_size`, 10K by default).
    #[serde(default = "default_event_cache_size")]
    pub event_cache_size: u32,

    /// How long a room nobody has used stays in memory before it is unloaded, checked once a
    /// minute. A room is loaded again, from the database, the next time it is used; the load
    /// reads the room's history, so a large room takes seconds to come back. Unset (the
    /// default), a room stays loaded for as long as the server runs. Synapse has no equivalent;
    /// it holds no room in memory between requests.
    #[serde(default)]
    pub idle_unload_after: Option<crate::duration::Duration>,
}

fn default_event_cache_size() -> u32 {
    1_000
}

impl Default for RoomsConfig {
    fn default() -> Self {
        Self {
            event_cache_size: default_event_cache_size(),
            idle_unload_after: None,
        }
    }
}

/// Retention of the sync feeds (`server.sync`). Both settings take effect at once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    /// How many feed entries to keep per user. A feed entry records that a room changed; one
    /// is written per room per update for rooms up to the fan-out threshold, and the entries
    /// a client has not yet synced are merged, so a client that keeps up adds at most one
    /// entry per room between two syncs. Below the kept entries, each room's last position
    /// stays, so a token older than this still finds every room that changed, each resumed
    /// from its last kept position or sent whole. `0` keeps every entry for ever. The
    /// feed is compacted once it has grown to twice this, so it holds between one and two
    /// times this many entries.
    #[serde(default = "default_feed_retention_entries")]
    pub feed_retention_entries: u64,

    /// How many entries to keep on the hot-room stream, which records one entry per update to
    /// a room over the fan-out threshold (500 joined members) instead of one per member. A
    /// token older than what is kept resumes each such room from its last kept position, or
    /// sends it whole. `0` keeps every entry for ever. Compacted once it has grown to twice
    /// this.
    #[serde(default = "default_hot_room_stream_retention_entries")]
    pub hot_room_stream_retention_entries: u64,
}

fn default_feed_retention_entries() -> u64 {
    10_000
}

fn default_hot_room_stream_retention_entries() -> u64 {
    100_000
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            feed_retention_entries: default_feed_retention_entries(),
            hot_room_stream_retention_entries: default_hot_room_stream_retention_entries(),
        }
    }
}

fn default_signing_key_path() -> PathBuf {
    PathBuf::from("./signing-keys")
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server_name: String::new(),
            public_baseurl: None,
            well_known_server: None,
            signing_key_path: default_signing_key_path(),
            admin_contact: None,
            unstable_features: BTreeMap::new(),
            sync: SyncConfig::default(),
            rooms: RoomsConfig::default(),
        }
    }
}

impl Validate for ServerConfig {
    fn validate(&self, prefix: &str, errors: &mut ValidationErrors) {
        if self.server_name.trim().is_empty() {
            errors.push(format!("{prefix}.server_name"), "must not be empty");
        } else if self.server_name.chars().any(char::is_whitespace) {
            errors.push(
                format!("{prefix}.server_name"),
                format!("{:?} must not contain whitespace", self.server_name),
            );
        } else if self.server_name.len() > 255 {
            errors.push(
                format!("{prefix}.server_name"),
                "must be at most 255 characters (DNS name limit)",
            );
        }
        if let Some(url) = &self.public_baseurl
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            errors.push(
                format!("{prefix}.public_baseurl"),
                format!("{url:?} must start with http:// or https://"),
            );
        }
        if let Some(delegate) = &self.well_known_server {
            // The empty string is a value: "publish no server document" (decision 0040), as
            // opposed to unset, which derives one from `public_baseurl`.
            let trimmed = delegate.trim();
            if trimmed.contains("://") || trimmed.contains('/') {
                errors.push(
                    format!("{prefix}.well_known_server"),
                    format!("{delegate:?} must be a host[:port], not a URL"),
                );
            } else if trimmed.chars().any(char::is_whitespace) {
                errors.push(
                    format!("{prefix}.well_known_server"),
                    format!("{delegate:?} must not contain whitespace"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace_names() {
        let mut errors = ValidationErrors::new();
        ServerConfig {
            server_name: "  ".into(),
            ..Default::default()
        }
        .validate("server", &mut errors);
        assert_eq!(errors.0[0].path, "server.server_name");
        assert_eq!(errors.0[0].message, "must not be empty");
    }

    #[test]
    fn rejects_baseurl_without_scheme() {
        let mut errors = ValidationErrors::new();
        ServerConfig {
            server_name: "example.org".into(),
            public_baseurl: Some("example.org".into()),
            ..Default::default()
        }
        .validate("server", &mut errors);
        assert_eq!(errors.0.len(), 1);
        assert!(errors.0[0].message.contains("http"));
    }

    #[test]
    fn accepts_a_normal_config() {
        let mut errors = ValidationErrors::new();
        ServerConfig {
            server_name: "matrix.example.org".into(),
            ..Default::default()
        }
        .validate("server", &mut errors);
        assert!(errors.is_empty());
    }

    #[test]
    fn rejects_a_well_known_delegation_written_as_a_url() {
        let mut errors = ValidationErrors::new();
        ServerConfig {
            server_name: "example.org".into(),
            well_known_server: Some("https://matrix.example.org:8448".into()),
            ..Default::default()
        }
        .validate("server", &mut errors);
        assert_eq!(errors.0.len(), 1);
        assert_eq!(errors.0[0].path, "server.well_known_server");
    }

    #[test]
    fn accepts_an_empty_well_known_delegation_as_the_document_turned_off() {
        let mut errors = ValidationErrors::new();
        ServerConfig {
            server_name: "example.org".into(),
            public_baseurl: Some("https://matrix.example.org".into()),
            well_known_server: Some(String::new()),
            ..Default::default()
        }
        .validate("server", &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn accepts_a_host_port_well_known_delegation() {
        let mut errors = ValidationErrors::new();
        ServerConfig {
            server_name: "example.org".into(),
            well_known_server: Some("matrix.example.org:8448".into()),
            ..Default::default()
        }
        .validate("server", &mut errors);
        assert!(errors.is_empty());
    }
}
