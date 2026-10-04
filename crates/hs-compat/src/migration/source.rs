//! Reading a Synapse database: [`SynapseSource`].
//!
//! Every query is a read, keyed and paged on a column that orders rows stably (a user's name,
//! a token's id), so a copy can stop between any two batches and pick up after the last row it
//! handled. Rows are read as `to_jsonb(row)`, so a column an older or newer Synapse lacks or adds
//! is simply absent or ignored rather than an error; the table and column names are those in
//! `docs/compat/synapse-importer-mapping.md`. Nothing is ever written to Synapse's database.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use hs_config::migration::SynapseSourceConfig;
use serde_json::Value;
use tokio_postgres::{Client, NoTls};

use super::MigrationError;
use super::model::{
    RoomShape, Stream, SynapseAccessToken, SynapseAccountData, SynapseBackupVersion,
    SynapseCrossSigning, SynapseDevice, SynapseDeviceKeys, SynapseEvent, SynapseFilter,
    SynapseMedia, SynapsePushRules, SynapsePusher, SynapseReceipt, SynapseRemoteJoin,
    SynapseRemoteMedia, SynapseRoom, SynapseRoomKey, SynapseUser,
};
use super::rows;

/// Tables a Synapse may not have (an older one, or one that never had the feature), each read
/// as empty when it is absent rather than failing the copy.
const OPTIONAL_TABLES: [&str; 17] = [
    "e2e_device_keys_json",
    "e2e_one_time_keys_json",
    "e2e_fallback_keys_json",
    "e2e_cross_signing_keys",
    "e2e_cross_signing_signatures",
    "e2e_room_keys_versions",
    "e2e_room_keys",
    "push_rules",
    "push_rules_enable",
    "pushers",
    "receipts_linearized",
    "user_filters",
    "partial_state_rooms",
    "event_to_state_groups",
    "state_groups_state",
    "state_group_edges",
    "remote_media_cache",
];

/// The key a room's events are paged on: `(topological_ordering, stream_ordering)`, which is
/// Synapse's own index on a room's events, and an order in which every event comes after the
/// events it cites (an event's depth is greater than its `prev_events`' and its auth events',
/// which are its ancestors). `stream_ordering` is unique, so the key is.
pub type EventKey = (i64, i64);

/// A connection to a Synapse deployment: its database, and its media store if mounted.
pub struct SynapseSource {
    client: Client,
    media_store: Option<PathBuf>,
    description: String,
    /// Which of [`OPTIONAL_TABLES`] this Synapse has.
    tables: HashSet<String>,
    /// `events.rejection_reason` exists (newer Synapses); older ones keep rejections only in
    /// `rejections`.
    rejection_reason: bool,
    /// [`SynapseSource::room_events`]'s query, prepared the first time it runs.
    room_events_statement: tokio::sync::OnceCell<tokio_postgres::Statement>,
    /// [`SynapseSource::room_event_ids`]'s query, prepared the first time it runs.
    room_event_ids_statement: tokio::sync::OnceCell<tokio_postgres::Statement>,
}

impl std::fmt::Debug for SynapseSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SynapseSource")
            .field("database", &self.description)
            .field("media_store", &self.media_store)
            .finish()
    }
}

fn db(e: tokio_postgres::Error) -> MigrationError {
    MigrationError::Source(e.to_string())
}

fn flag(row: &Value, key: &str) -> bool {
    match row.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().is_some_and(|n| n != 0),
        _ => false,
    }
}

fn text(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn number(row: &Value, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)
}

fn unsigned(row: &Value, key: &str) -> Option<u64> {
    number(row, key).and_then(|n| u64::try_from(n).ok())
}

fn parse_row(raw: &str) -> Result<Value, MigrationError> {
    serde_json::from_str(raw).map_err(|e| MigrationError::Source(format!("unreadable row: {e}")))
}

/// An account and its profile, as one JSON row.
const USER_SELECT: &str = "SELECT (to_jsonb(u) || jsonb_build_object('displayname', p.displayname, 'avatar_url', p.avatar_url))::text \
     FROM users u LEFT JOIN profiles p ON p.user_id = split_part(substr(u.name, 2), ':', 1)";

fn parse_user(raw: &str) -> Result<SynapseUser, MigrationError> {
    let r = parse_row(raw)?;
    Ok(SynapseUser {
        user_id: text(&r, "name").unwrap_or_default(),
        password_hash: text(&r, "password_hash").filter(|h| !h.is_empty()),
        created_at_ms: unsigned(&r, "creation_ts")
            .unwrap_or(0)
            .saturating_mul(1000),
        admin: flag(&r, "admin"),
        guest: flag(&r, "is_guest"),
        deactivated: flag(&r, "deactivated"),
        shadow_banned: flag(&r, "shadow_banned"),
        locked: flag(&r, "locked"),
        appservice_id: text(&r, "appservice_id"),
        user_type: text(&r, "user_type"),
        displayname: text(&r, "displayname"),
        avatar_url: text(&r, "avatar_url"),
    })
}

/// Where Synapse keeps a local media item's file, relative to its `media_store_path`:
/// `local_content/ab/cd/efgh...` for media id `abcdefgh...`.
#[must_use]
pub fn local_content_path(media_id: &str) -> Option<PathBuf> {
    if media_id.len() < 5 || !media_id.is_ascii() || media_id.contains(['/', '\\', '.']) {
        return None;
    }
    Some(
        Path::new("local_content")
            .join(&media_id[0..2])
            .join(&media_id[2..4])
            .join(&media_id[4..]),
    )
}

/// Where Synapse keeps its cached copy of another server's media item, relative to its
/// `media_store_path`: `remote_content/<origin>/ab/cd/efgh...` for `mxc://<origin>/abcdefgh...`
/// (`hs_media::synapse_layout`'s `RemoteContent` subtree, whose classifier reads this path back;
/// `hs-cli`'s `migration` tests hold the two to each other). `None` for an origin or a media id
/// that is not a plain path component, so nothing escapes the store.
#[must_use]
pub fn remote_content_path(origin: &str, media_id: &str) -> Option<PathBuf> {
    if origin.is_empty()
        || !origin.is_ascii()
        || origin.starts_with('.')
        || origin.contains(['/', '\\', ' '])
    {
        return None;
    }
    let local = local_content_path(media_id)?;
    Some(
        Path::new("remote_content")
            .join(origin)
            .join(local.strip_prefix("local_content").ok()?),
    )
}

impl SynapseSource {
    /// Connects to the database `config` names. The connection is driven on the runtime until
    /// this source is dropped.
    ///
    /// # Errors
    /// [`MigrationError::Source`] when the database cannot be reached or does not look like
    /// Synapse's (no `users` or `events` table).
    pub async fn connect(config: &SynapseSourceConfig) -> Result<Self, MigrationError> {
        let (client, connection) =
            tokio_postgres::connect(&config.database.connection_string(), NoTls)
                .await
                .map_err(|e| {
                    MigrationError::Source(format!(
                        "could not connect to {}: {e}",
                        config.database.describe()
                    ))
                })?;
        let description = config.database.describe();
        let for_log = description.clone();
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::warn!(source = %for_log, %error, "the connection to the Synapse database ended");
            }
        });
        let mut source = Self {
            client,
            media_store: config.media_store_path.clone(),
            description,
            tables: HashSet::new(),
            rejection_reason: false,
            room_events_statement: tokio::sync::OnceCell::new(),
            room_event_ids_statement: tokio::sync::OnceCell::new(),
        };
        let optional: Vec<&str> = OPTIONAL_TABLES.to_vec();
        source.tables = source
            .client
            .query(
                "SELECT table_name::text FROM information_schema.tables \
                 WHERE table_schema = current_schema() AND table_name = ANY($1)",
                &[&optional],
            )
            .await
            .map_err(db)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        source.rejection_reason = source
            .client
            .query_one(
                "SELECT count(*) FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND table_name = 'events' \
                 AND column_name = 'rejection_reason'",
                &[],
            )
            .await
            .map_err(db)?
            .get::<_, i64>(0)
            > 0;
        for table in ["users", "events", "event_json", "rooms"] {
            let found: i64 = source
                .client
                .query_one(
                    "SELECT count(*) FROM information_schema.tables WHERE table_name = $1",
                    &[&table],
                )
                .await
                .map_err(db)?
                .get(0);
            if found == 0 {
                return Err(MigrationError::Source(format!(
                    "{} has no {table} table: it is not a Synapse database",
                    source.description
                )));
            }
        }
        Ok(source)
    }

    /// The database, without its password.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The media store, if one is mounted.
    #[must_use]
    pub fn media_store(&self) -> Option<&Path> {
        self.media_store.as_deref()
    }

    /// The server name Synapse's accounts are under (from its oldest account), or `None` for a
    /// database with no accounts.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn server_name(&self) -> Result<Option<String>, MigrationError> {
        let row = self
            .client
            .query_opt(
                "SELECT name FROM users WHERE name IS NOT NULL ORDER BY creation_ts NULLS LAST, name LIMIT 1",
                &[],
            )
            .await
            .map_err(db)?;
        Ok(row.and_then(|row| {
            let name: String = row.get(0);
            name.split_once(':').map(|(_, server)| server.to_owned())
        }))
    }

    /// How many rows the stream has in Synapse, skipped kinds included.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn count(&self, stream: Stream) -> Result<u64, MigrationError> {
        let needs: &[&str] = match stream {
            Stream::CrossSigning => &["e2e_cross_signing_keys"],
            Stream::KeyBackups => &["e2e_room_keys_versions"],
            Stream::PushRules => &["push_rules", "push_rules_enable"],
            Stream::Pushers => &["pushers"],
            Stream::Filters => &["user_filters"],
            Stream::Receipts => &["receipts_linearized"],
            Stream::RemoteMedia => &["remote_media_cache"],
            _ => &[],
        };
        if needs.iter().any(|t| !self.has(t)) {
            return Ok(0);
        }
        let devices = self.e2e_device_union();
        let sql = match stream {
            Stream::Users => "SELECT count(*) FROM users".to_owned(),
            Stream::Devices => "SELECT count(*) FROM devices".to_owned(),
            Stream::AccessTokens => "SELECT count(*) FROM access_tokens".to_owned(),
            Stream::AccountData => {
                "SELECT (SELECT count(*) FROM account_data) + (SELECT count(*) FROM room_account_data) \
                 + (SELECT count(*) FROM (SELECT DISTINCT user_id, room_id FROM room_tags) t)"
                    .to_owned()
            }
            Stream::E2eKeys => format!("SELECT count(*) FROM ({devices}) d"),
            Stream::CrossSigning => "SELECT count(DISTINCT k.user_id) FROM e2e_cross_signing_keys k \
                 JOIN users u ON u.name = k.user_id"
                .to_owned(),
            Stream::KeyBackups => "SELECT count(*) FROM e2e_room_keys_versions".to_owned(),
            Stream::PushRules => "SELECT count(*) FROM (SELECT user_name FROM push_rules \
                 UNION SELECT user_name FROM push_rules_enable) p"
                .to_owned(),
            Stream::Pushers => "SELECT count(*) FROM pushers".to_owned(),
            Stream::Filters => "SELECT count(*) FROM user_filters".to_owned(),
            Stream::Rooms => "SELECT count(*) FROM rooms".to_owned(),
            Stream::Receipts => "SELECT count(*) FROM receipts_linearized".to_owned(),
            Stream::Media => "SELECT count(*) FROM local_media_repository".to_owned(),
            Stream::RemoteMedia => "SELECT count(*) FROM remote_media_cache".to_owned(),
        };
        let n: i64 = self.client.query_one(&sql, &[]).await.map_err(db)?.get(0);
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Whether this Synapse has `table` (one of [`OPTIONAL_TABLES`]).
    fn has(&self, table: &str) -> bool {
        self.tables.contains(table)
    }

    /// `(user_id, device_id)` of every device with end-to-end keys of any kind, as SQL.
    fn e2e_device_union(&self) -> String {
        let parts: Vec<String> = [
            "e2e_device_keys_json",
            "e2e_one_time_keys_json",
            "e2e_fallback_keys_json",
        ]
        .into_iter()
        .filter(|t| self.has(t))
        .map(|t| format!("SELECT user_id, device_id FROM {t}"))
        .collect();
        if parts.is_empty() {
            "SELECT NULL::text AS user_id, NULL::text AS device_id WHERE false".to_owned()
        } else {
            parts.join(" UNION ")
        }
    }

    /// Accounts after `after` (a user id), in name order.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn users(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SynapseUser>, MigrationError> {
        let sql = format!(
            "{USER_SELECT} WHERE u.name IS NOT NULL AND ($1::text IS NULL OR u.name > $1) ORDER BY u.name LIMIT $2"
        );
        let rows = self
            .client
            .query(&sql, &[&after, &limit])
            .await
            .map_err(db)?;
        rows.iter().map(|row| parse_user(row.get(0))).collect()
    }

    /// One account, for verification.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn user(&self, user_id: &str) -> Result<Option<SynapseUser>, MigrationError> {
        let sql = format!("{USER_SELECT} WHERE u.name = $1");
        self.client
            .query_opt(&sql, &[&user_id])
            .await
            .map_err(db)?
            .map(|row| parse_user(row.get(0)))
            .transpose()
    }

    /// Devices after `after` (`(user id, device id)`).
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn devices(
        &self,
        after: Option<(&str, &str)>,
        limit: i64,
    ) -> Result<Vec<SynapseDevice>, MigrationError> {
        let (user, device) = after.unzip();
        let rows = self
            .client
            .query(
                "SELECT to_jsonb(d)::text FROM devices d \
                 WHERE $1::text IS NULL OR (d.user_id, d.device_id) > ($1::text, $2::text) \
                 ORDER BY d.user_id, d.device_id LIMIT $3",
                &[&user, &device, &limit],
            )
            .await
            .map_err(db)?;
        rows.iter()
            .map(|row| {
                let r = parse_row(row.get(0))?;
                Ok(SynapseDevice {
                    user_id: text(&r, "user_id").unwrap_or_default(),
                    device_id: text(&r, "device_id").unwrap_or_default(),
                    display_name: text(&r, "display_name"),
                    last_seen_ms: unsigned(&r, "last_seen"),
                    last_seen_ip: text(&r, "ip"),
                    hidden: flag(&r, "hidden"),
                })
            })
            .collect()
    }

    /// Access tokens after row id `after`.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn access_tokens(
        &self,
        after: Option<i64>,
        limit: i64,
    ) -> Result<Vec<SynapseAccessToken>, MigrationError> {
        let rows = self
            .client
            .query(
                "SELECT to_jsonb(t)::text FROM access_tokens t WHERE $1::bigint IS NULL OR t.id > $1 \
                 ORDER BY t.id LIMIT $2",
                &[&after, &limit],
            )
            .await
            .map_err(db)?;
        rows.iter()
            .map(|row| {
                let r = parse_row(row.get(0))?;
                Ok(SynapseAccessToken {
                    id: number(&r, "id").unwrap_or(0),
                    user_id: text(&r, "user_id").unwrap_or_default(),
                    device_id: text(&r, "device_id"),
                    token: text(&r, "token").unwrap_or_default(),
                    valid_until_ms: unsigned(&r, "valid_until_ms"),
                    puppets_user_id: text(&r, "puppets_user_id"),
                })
            })
            .collect()
    }

    /// Account data after `after` (the key [`account_data_key`] makes), global first, then per
    /// room, then each room's tags as one `m.tag`.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn account_data(
        &self,
        after: Option<&[String; 4]>,
        limit: i64,
    ) -> Result<Vec<(SynapseAccountData, [String; 4])>, MigrationError> {
        let (kind, user, room, data_type) = match after {
            Some([k, u, r, t]) => (
                Some(k.as_str()),
                Some(u.as_str()),
                Some(r.as_str()),
                Some(t.as_str()),
            ),
            None => (None, None, None, None),
        };
        let rows = self
            .client
            .query(
                "SELECT k, user_id, room_id, data_type, content FROM ( \
                   SELECT 'g'::text AS k, user_id, ''::text AS room_id, account_data_type AS data_type, content FROM account_data \
                   UNION ALL SELECT 'r'::text, user_id, room_id, account_data_type, content FROM room_account_data \
                   UNION ALL SELECT 't'::text, user_id, room_id, 'm.tag'::text, \
                     json_build_object('tags', json_object_agg(tag, content::json))::text FROM room_tags GROUP BY user_id, room_id \
                 ) x WHERE $1::text IS NULL OR (k, user_id, room_id, data_type) > ($1::text, $2::text, $3::text, $4::text) \
                 ORDER BY k, user_id, room_id, data_type LIMIT $5",
                &[&kind, &user, &room, &data_type, &limit],
            )
            .await
            .map_err(db)?;
        rows.iter()
            .map(|row| {
                let key: [String; 4] = [row.get(0), row.get(1), row.get(2), row.get(3)];
                let content: String = row.get(4);
                let content = serde_json::from_str(&content).map_err(|e| {
                    MigrationError::Source(format!(
                        "{}'s {} holds unreadable JSON: {e}",
                        key[1], key[3]
                    ))
                })?;
                Ok((
                    SynapseAccountData {
                        user_id: key[1].clone(),
                        room_id: (!key[2].is_empty()).then(|| key[2].clone()),
                        data_type: key[3].clone(),
                        content,
                    },
                    key,
                ))
            })
            .collect()
    }

    /// Room ids after `after`, in order.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn room_ids(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<String>, MigrationError> {
        let rows = self
            .client
            .query(
                "SELECT room_id FROM rooms WHERE $1::text IS NULL OR room_id > $1 ORDER BY room_id LIMIT $2",
                &[&after, &limit],
            )
            .await
            .map_err(db)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// One room, with its aliases and redactions; its events are read with
    /// [`SynapseSource::room_events`].
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn room(&self, room_id: &str) -> Result<Option<SynapseRoom>, MigrationError> {
        let Some(row) = self
            .client
            .query_opt(
                "SELECT to_jsonb(r)::text FROM rooms r WHERE r.room_id = $1",
                &[&room_id],
            )
            .await
            .map_err(db)?
        else {
            return Ok(None);
        };
        let r = parse_row(row.get(0))?;
        let aliases = self
            .client
            .query(
                "SELECT room_alias, creator FROM room_aliases WHERE room_id = $1 ORDER BY room_alias",
                &[&room_id],
            )
            .await
            .map_err(db)?
            .iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, Option<String>>(1)))
            .collect();
        let redactions = self
            .client
            .query(
                "SELECT r.event_id, r.redacts FROM redactions r JOIN events e ON e.event_id = r.event_id \
                 WHERE e.room_id = $1 ORDER BY e.stream_ordering",
                &[&room_id],
            )
            .await
            .map_err(db)?
            .iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
            .collect();
        Ok(Some(SynapseRoom {
            room_id: room_id.to_owned(),
            room_version: text(&r, "room_version").unwrap_or_else(|| "1".to_owned()),
            is_public: flag(&r, "is_public"),
            aliases,
            redactions,
        }))
    }

    /// SQL for "this event (`e`) was rejected".
    fn rejected_sql(&self) -> &'static str {
        if self.rejection_reason {
            "(e.rejection_reason IS NOT NULL \
              OR EXISTS (SELECT 1 FROM rejections x WHERE x.event_id = e.event_id))"
        } else {
            "EXISTS (SELECT 1 FROM rejections x WHERE x.event_id = e.event_id)"
        }
    }

    /// How a room's history is held: how many of its events are part of it, how many are
    /// outliers or rejected, and whether its `m.room.create` is part of it.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn room_shape(&self, room_id: &str) -> Result<RoomShape, MigrationError> {
        let rejected = self.rejected_sql();
        let row = self
            .client
            .query_one(
                &format!(
                    "SELECT count(*) FILTER (WHERE NOT x.outlier AND NOT x.rejected), \
                       count(*) FILTER (WHERE x.outlier AND NOT x.rejected), \
                       count(*) FILTER (WHERE x.rejected), \
                       coalesce(bool_or(x.create_event AND NOT x.outlier AND NOT x.rejected), false) \
                     FROM (SELECT e.outlier, {rejected} AS rejected, \
                             (e.type = 'm.room.create' AND e.state_key = '') AS create_event \
                           FROM events e WHERE e.room_id = $1) x"
                ),
                &[&room_id],
            )
            .await
            .map_err(db)?;
        let n = |i: usize| u64::try_from(row.get::<_, i64>(i)).unwrap_or(0);
        Ok(RoomShape {
            history: n(0),
            outliers: n(1),
            rejected: n(2),
            has_create: row.get(3),
        })
    }

    /// The next `limit` events of a room's history after `after`, in [`EventKey`] order:
    /// outliers and rejected events left out (they are not part of it), each event as Synapse
    /// stored it without its `unsigned`. With `since`, only events Synapse stored after that
    /// stream position (a room joined over federation: what came after the join).
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or an event whose JSON is unreadable.
    pub async fn room_events(
        &self,
        room_id: &str,
        after: Option<EventKey>,
        limit: i64,
        since: Option<i64>,
    ) -> Result<Vec<(SynapseEvent, EventKey)>, MigrationError> {
        let (topological, stream) = after.unzip();
        // Prepared once: planning this query costs more than running it for a page.
        let statement = self
            .room_events_statement
            .get_or_try_init(|| async {
                let rejected = self.rejected_sql();
                self.client
                    .prepare(&format!(
                        "SELECT e.event_id, e.depth, coalesce(e.topological_ordering, e.depth), \
                           coalesce(e.stream_ordering, 0), j.json \
                         FROM events e JOIN event_json j ON j.event_id = e.event_id \
                         WHERE e.room_id = $1 AND NOT e.outlier AND NOT {rejected} \
                           AND ($2::bigint IS NULL \
                                OR (e.topological_ordering, e.stream_ordering) > ($2::bigint, $3::bigint)) \
                           AND ($5::bigint IS NULL OR e.stream_ordering > $5::bigint) \
                         ORDER BY e.topological_ordering, e.stream_ordering LIMIT $4"
                    ))
                    .await
            })
            .await
            .map_err(db)?;
        let rows = self
            .client
            .query(
                statement,
                &[&room_id, &topological, &stream, &limit, &since],
            )
            .await
            .map_err(db)?;
        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            let event_id: String = row.get(0);
            let raw: String = row.get(4);
            let mut json: Value = serde_json::from_str(&raw).map_err(|e| {
                MigrationError::Source(format!("event {event_id} holds unreadable JSON: {e}"))
            })?;
            if let Some(object) = json.as_object_mut() {
                object.remove("unsigned");
            }
            let key = (row.get::<_, i64>(2), row.get::<_, i64>(3));
            events.push((
                SynapseEvent {
                    event_id,
                    json,
                    depth: row.get(1),
                    stream_ordering: key.1,
                    outlier: false,
                    rejected: false,
                    json_bytes: raw.len() as u64,
                },
                key,
            ));
        }
        Ok(events)
    }

    /// The ids of a room's history after `after`, in [`EventKey`] order, for verification.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn room_event_ids(
        &self,
        room_id: &str,
        after: Option<EventKey>,
        limit: i64,
        since: Option<i64>,
    ) -> Result<Vec<(String, EventKey)>, MigrationError> {
        let (topological, stream) = after.unzip();
        let statement = self
            .room_event_ids_statement
            .get_or_try_init(|| async {
                let rejected = self.rejected_sql();
                self.client
                    .prepare(&format!(
                        "SELECT e.event_id, coalesce(e.topological_ordering, e.depth), \
                           coalesce(e.stream_ordering, 0) \
                         FROM events e \
                         WHERE e.room_id = $1 AND NOT e.outlier AND NOT {rejected} \
                           AND ($2::bigint IS NULL \
                                OR (e.topological_ordering, e.stream_ordering) > ($2::bigint, $3::bigint)) \
                           AND ($5::bigint IS NULL OR e.stream_ordering > $5::bigint) \
                         ORDER BY e.topological_ordering, e.stream_ordering LIMIT $4"
                    ))
                    .await
            })
            .await
            .map_err(db)?;
        Ok(self
            .client
            .query(
                statement,
                &[&room_id, &topological, &stream, &limit, &since],
            )
            .await
            .map_err(db)?
            .iter()
            .map(|row| (row.get(0), (row.get(1), row.get(2))))
            .collect())
    }

    /// Events by id, as Synapse stored them (without `unsigned`); those it does not hold are left
    /// out.
    async fn events_by_id(&self, ids: &[String]) -> Result<Vec<SynapseEvent>, MigrationError> {
        let rejected = self.rejected_sql();
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(1000) {
            let rows = self
                .client
                .query(
                    &format!(
                        "SELECT e.event_id, e.depth, coalesce(e.stream_ordering, 0), e.outlier, \
                           {rejected}, j.json \
                         FROM events e JOIN event_json j ON j.event_id = e.event_id \
                         WHERE e.event_id = ANY($1)"
                    ),
                    &[&chunk],
                )
                .await
                .map_err(db)?;
            for row in &rows {
                let event_id: String = row.get(0);
                let raw: String = row.get(5);
                let mut json: Value = serde_json::from_str(&raw).map_err(|e| {
                    MigrationError::Source(format!("event {event_id} holds unreadable JSON: {e}"))
                })?;
                if let Some(object) = json.as_object_mut() {
                    object.remove("unsigned");
                }
                out.push(SynapseEvent {
                    event_id,
                    json,
                    depth: row.get(1),
                    stream_ordering: row.get(2),
                    outlier: row.get(3),
                    rejected: row.get(4),
                    json_bytes: raw.len() as u64,
                });
            }
        }
        Ok(out)
    }

    /// How a room that this server's users joined over federation came to be held here: the
    /// first join of one of this server's own accounts (made by the account itself) that is part
    /// of the room's history, the room's state before it, and the auth chain of that state and
    /// of the join -- what the resident server's `send_join` answered, which Synapse keeps as
    /// outliers. `Ok(Err(why))` when there is no such join to start from: this server's users
    /// were only invited, or Synapse has not finished joining (a faster join still resyncing
    /// its state).
    ///
    /// Synapse records the state *after* a state event (`event_to_state_groups`, a chain of
    /// deltas in `state_groups_state` along `state_group_edges`); the state before the join is
    /// that, with the joining account's membership put back to what the join's own
    /// `auth_events` say it was (or taken out, if it had none).
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or unreadable event JSON.
    pub async fn remote_join(
        &self,
        room_id: &str,
    ) -> Result<Result<SynapseRemoteJoin, String>, MigrationError> {
        if self.has("partial_state_rooms") {
            let partial: i64 = self
                .client
                .query_one(
                    "SELECT count(*) FROM partial_state_rooms WHERE room_id = $1",
                    &[&room_id],
                )
                .await
                .map_err(db)?
                .get(0);
            if partial > 0 {
                return Ok(Err(
                    "Synapse has not finished joining it (a faster join is still fetching its \
                     state): copy it once Synapse has"
                        .to_owned(),
                ));
            }
        }
        if [
            "event_to_state_groups",
            "state_groups_state",
            "state_group_edges",
        ]
        .iter()
        .any(|t| !self.has(t))
        {
            return Ok(Err(
                "this Synapse database has no state groups to find the state at its join in"
                    .to_owned(),
            ));
        }
        let rejected = self.rejected_sql();
        let Some(row) = self
            .client
            .query_opt(
                &format!(
                    "SELECT e.event_id, coalesce(e.topological_ordering, e.depth), \
                       coalesce(e.stream_ordering, 0) \
                     FROM events e JOIN event_json j ON j.event_id = e.event_id \
                       JOIN users u ON u.name = e.state_key \
                     WHERE e.room_id = $1 AND e.type = 'm.room.member' AND e.sender = e.state_key \
                       AND NOT e.outlier AND NOT {rejected} \
                       AND (j.json::jsonb -> 'content' ->> 'membership') = 'join' \
                     ORDER BY e.stream_ordering LIMIT 1"
                ),
                &[&room_id],
            )
            .await
            .map_err(db)?
        else {
            return Ok(Err(
                "it was made on another server, and none of this server's users joined it \
                 (they were only invited, or have left): there is no join to start it from"
                    .to_owned(),
            ));
        };
        let join_id: String = row.get(0);
        let join_key: EventKey = (row.get(1), row.get(2));
        let Some(join) = self
            .events_by_id(std::slice::from_ref(&join_id))
            .await?
            .into_iter()
            .next()
        else {
            return Ok(Err(format!("its join {join_id} has no JSON in Synapse")));
        };
        let Some(group) = self
            .client
            .query_opt(
                "SELECT state_group FROM event_to_state_groups WHERE event_id = $1",
                &[&join_id],
            )
            .await
            .map_err(db)?
        else {
            return Ok(Err(format!(
                "Synapse holds no state for its join {join_id}"
            )));
        };
        let group: i64 = group.get(0);
        let mut state: std::collections::BTreeMap<(String, String), String> = self
            .client
            .query(
                "WITH RECURSIVE chain(state_group, hops) AS ( \
                   SELECT $1::bigint, 0 \
                   UNION ALL \
                   SELECT e.prev_state_group, c.hops + 1 FROM state_group_edges e \
                     JOIN chain c ON e.state_group = c.state_group \
                 ) \
                 SELECT DISTINCT ON (s.type, s.state_key) s.type, s.state_key, s.event_id \
                 FROM state_groups_state s JOIN chain c ON s.state_group = c.state_group \
                 ORDER BY s.type, s.state_key, c.hops",
                &[&group],
            )
            .await
            .map_err(db)?
            .iter()
            .map(|row| ((row.get(0), row.get(1)), row.get(2)))
            .collect();
        // Before the join: the joining account's membership as the join's auth events cite it.
        let user = join.json["state_key"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let member_key = ("m.room.member".to_owned(), user.clone());
        state.remove(&member_key);
        let cited: Vec<String> = join.json["auth_events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        for event in self.events_by_id(&cited).await? {
            if event.json["type"] == "m.room.member" && event.json["state_key"] == user.as_str() {
                state.insert(member_key.clone(), event.event_id.clone());
            }
        }
        let state_ids: Vec<String> = state.values().cloned().collect();
        let state_events = self.events_by_id(&state_ids).await?;
        if state_events.len() != state_ids.len() {
            return Ok(Err(format!(
                "Synapse is missing {} of the {} events of its state at the join",
                state_ids.len() - state_events.len(),
                state_ids.len()
            )));
        }
        // The auth chain of that state and of the join, followed through `auth_events`.
        let mut seen: HashSet<String> = state_ids.iter().cloned().collect();
        seen.insert(join_id.clone());
        let mut frontier: Vec<String> = Vec::new();
        for event in state_events.iter().chain(std::iter::once(&join)) {
            for id in event.json["auth_events"].as_array().into_iter().flatten() {
                if let Some(id) = id.as_str()
                    && seen.insert(id.to_owned())
                {
                    frontier.push(id.to_owned());
                }
            }
        }
        let mut auth_chain = Vec::new();
        while !frontier.is_empty() {
            let found = self.events_by_id(&frontier).await?;
            frontier.clear();
            for event in &found {
                for id in event.json["auth_events"].as_array().into_iter().flatten() {
                    if let Some(id) = id.as_str()
                        && seen.insert(id.to_owned())
                    {
                        frontier.push(id.to_owned());
                    }
                }
            }
            auth_chain.extend(found);
        }
        // Events the state cites through `auth_events` that are themselves state are in
        // `state` already; the chain is everything else.
        Ok(Ok(SynapseRemoteJoin {
            join,
            join_key,
            state: state_events,
            auth_chain,
        }))
    }

    /// How many events of a room's history Synapse stored after stream position `since`.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn history_since(&self, room_id: &str, since: i64) -> Result<u64, MigrationError> {
        let rejected = self.rejected_sql();
        let n: i64 = self
            .client
            .query_one(
                &format!(
                    "SELECT count(*) FROM events e WHERE e.room_id = $1 AND NOT e.outlier \
                       AND NOT {rejected} AND e.stream_ordering > $2"
                ),
                &[&room_id, &since],
            )
            .await
            .map_err(db)?
            .get(0);
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// A room's current state as Synapse has it: `(type, state_key) -> event_id`.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn current_state(
        &self,
        room_id: &str,
    ) -> Result<std::collections::BTreeMap<(String, String), String>, MigrationError> {
        Ok(self
            .client
            .query(
                "SELECT type, state_key, event_id FROM current_state_events WHERE room_id = $1",
                &[&room_id],
            )
            .await
            .map_err(db)?
            .iter()
            .map(|row| ((row.get(0), row.get(1)), row.get(2)))
            .collect())
    }

    /// Local media after `after` (a media id). URL-preview cache entries are left out.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn media(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<(SynapseMedia, bool)>, MigrationError> {
        let rows = self
            .client
            .query(
                "SELECT to_jsonb(m)::text FROM local_media_repository m \
                 WHERE m.media_id IS NOT NULL AND ($1::text IS NULL OR m.media_id > $1) ORDER BY m.media_id LIMIT $2",
                &[&after, &limit],
            )
            .await
            .map_err(db)?;
        rows.iter()
            .map(|row| {
                let r = parse_row(row.get(0))?;
                let url_cache = text(&r, "url_cache").is_some();
                Ok((
                    SynapseMedia {
                        media_id: text(&r, "media_id").unwrap_or_default(),
                        content_type: text(&r, "media_type"),
                        length: unsigned(&r, "media_length"),
                        created_ms: unsigned(&r, "created_ts").unwrap_or(0),
                        upload_name: text(&r, "upload_name"),
                        uploader: text(&r, "user_id"),
                        quarantined_by: text(&r, "quarantined_by"),
                        safe_from_quarantine: flag(&r, "safe_from_quarantine"),
                    },
                    url_cache,
                ))
            })
            .collect()
    }

    /// A local media item's file from the media store: `Ok(None)` when no media store is
    /// mounted or the file is not there.
    ///
    /// # Errors
    /// [`MigrationError::Source`] when the file exists but cannot be read.
    pub async fn media_bytes(&self, media_id: &str) -> Result<Option<Vec<u8>>, MigrationError> {
        let (Some(root), Some(relative)) = (&self.media_store, local_content_path(media_id)) else {
            return Ok(None);
        };
        let path = root.join(relative);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MigrationError::Source(format!(
                "could not read {}: {e}",
                path.display()
            ))),
        }
    }

    /// Other servers' media Synapse had cached, after `after` (an origin and a media id), in
    /// `(origin, media_id)` order. Empty for a Synapse without the table.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn remote_media(
        &self,
        after: Option<(&str, &str)>,
        limit: i64,
    ) -> Result<Vec<SynapseRemoteMedia>, MigrationError> {
        if !self.has("remote_media_cache") {
            return Ok(Vec::new());
        }
        let (after_origin, after_id) = match after {
            Some((origin, id)) => (Some(origin), Some(id)),
            None => (None, None),
        };
        let rows = self
            .client
            .query(
                "SELECT to_jsonb(m)::text FROM remote_media_cache m \
                 WHERE m.media_origin IS NOT NULL AND m.media_id IS NOT NULL \
                 AND ($1::text IS NULL OR (m.media_origin, m.media_id) > ($1, $2)) \
                 ORDER BY m.media_origin, m.media_id LIMIT $3",
                &[&after_origin, &after_id, &limit],
            )
            .await
            .map_err(db)?;
        rows.iter()
            .map(|row| {
                let r = parse_row(row.get(0))?;
                Ok(SynapseRemoteMedia {
                    origin: text(&r, "media_origin").unwrap_or_default(),
                    media_id: text(&r, "media_id").unwrap_or_default(),
                    content_type: text(&r, "media_type"),
                    length: unsigned(&r, "media_length"),
                    created_ms: unsigned(&r, "created_ts").unwrap_or(0),
                    upload_name: text(&r, "upload_name"),
                    last_access_ms: unsigned(&r, "last_access_ts"),
                    quarantined_by: text(&r, "quarantined_by"),
                })
            })
            .collect()
    }

    /// A cached remote media item's file from the media store: `Ok(None)` when no media store
    /// is mounted, the item's origin or id cannot name a file, or the file is not there.
    ///
    /// # Errors
    /// [`MigrationError::Source`] when the file exists but cannot be read.
    pub async fn remote_media_bytes(
        &self,
        origin: &str,
        media_id: &str,
    ) -> Result<Option<Vec<u8>>, MigrationError> {
        let (Some(root), Some(relative)) =
            (&self.media_store, remote_content_path(origin, media_id))
        else {
            return Ok(None);
        };
        let path = root.join(relative);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MigrationError::Source(format!(
                "could not read {}: {e}",
                path.display()
            ))),
        }
    }

    /// Whether a cached remote media item's file is in the mounted media store (without reading
    /// it): what verification uses to tell an entry the copy had to leave out from one it
    /// should have copied.
    pub async fn remote_media_file_exists(&self, origin: &str, media_id: &str) -> bool {
        let (Some(root), Some(relative)) =
            (&self.media_store, remote_content_path(origin, media_id))
        else {
            return false;
        };
        tokio::fs::metadata(root.join(relative))
            .await
            .map(|m| m.is_file())
            .unwrap_or(false)
    }

    /// A random sample of up to `limit` keys of `stream`'s rows, for verification's field-by-field
    /// comparison. Streams verification compares whole have no sample (an empty list).
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn sample_keys(
        &self,
        stream: Stream,
        limit: i64,
    ) -> Result<Vec<String>, MigrationError> {
        let sql = match stream {
            Stream::E2eKeys
            | Stream::CrossSigning
            | Stream::KeyBackups
            | Stream::PushRules
            | Stream::Pushers
            | Stream::Filters
            | Stream::Receipts
            | Stream::RemoteMedia => return Ok(Vec::new()),
            Stream::Users => {
                "SELECT name FROM users WHERE name IS NOT NULL ORDER BY random() LIMIT $1"
            }
            Stream::Devices => {
                "SELECT user_id || ' ' || device_id FROM devices WHERE NOT coalesce(hidden, false) ORDER BY random() LIMIT $1"
            }
            Stream::AccessTokens => {
                "SELECT id::text FROM access_tokens WHERE puppets_user_id IS NULL ORDER BY random() LIMIT $1"
            }
            Stream::AccountData => {
                "SELECT user_id || ' ' || account_data_type FROM account_data ORDER BY random() LIMIT $1"
            }
            Stream::Rooms => "SELECT room_id FROM rooms ORDER BY random() LIMIT $1",
            Stream::Media => {
                "SELECT media_id FROM local_media_repository WHERE media_id IS NOT NULL ORDER BY random() LIMIT $1"
            }
        };
        Ok(self
            .client
            .query(sql, &[&limit])
            .await
            .map_err(db)?
            .iter()
            .map(|r| r.get(0))
            .collect())
    }

    /// One access token by row id, for verification.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn access_token(
        &self,
        id: i64,
    ) -> Result<Option<SynapseAccessToken>, MigrationError> {
        Ok(self
            .access_tokens(Some(id - 1), 1)
            .await?
            .into_iter()
            .find(|t| t.id == id))
    }

    /// One piece of global account data, for verification.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or unreadable JSON.
    pub async fn global_account_data(
        &self,
        user_id: &str,
        data_type: &str,
    ) -> Result<Option<Value>, MigrationError> {
        let Some(row) = self
            .client
            .query_opt(
                "SELECT content FROM account_data WHERE user_id = $1 AND account_data_type = $2",
                &[&user_id, &data_type],
            )
            .await
            .map_err(db)?
        else {
            return Ok(None);
        };
        let raw: String = row.get(0);
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| MigrationError::Source(format!("unreadable account data: {e}")))
    }

    /// `SELECT to_jsonb(t)::text FROM <sql>` rows, parsed; none when `table` is absent.
    async fn json_rows(
        &self,
        table: &str,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<Value>, MigrationError> {
        if !self.has(table) {
            return Ok(Vec::new());
        }
        self.client
            .query(sql, params)
            .await
            .map_err(db)?
            .iter()
            .map(|row| parse_row(row.get(0)))
            .collect()
    }

    /// The signatures Synapse holds on `user_id`'s keys and devices, by what they sign (a
    /// device id, or a cross-signing key's public key).
    async fn signatures_on(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, rows::HeldSignature)>, MigrationError> {
        self.json_rows(
            "e2e_cross_signing_signatures",
            "SELECT to_jsonb(s)::text FROM e2e_cross_signing_signatures s \
             WHERE s.target_user_id = $1 ORDER BY s.target_device_id, s.user_id, s.key_id",
            &[&user_id],
        )
        .await?
        .iter()
        .map(|row| {
            rows::signature(row)
                .map(|(_, target, held)| (target, held))
                .map_err(MigrationError::Source)
        })
        .collect()
    }

    /// End-to-end keys of the devices after `after` (`(user id, device id)`): every device of
    /// an account of this server with identity, one-time or fallback keys.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or unreadable key JSON.
    pub async fn e2e_device_keys(
        &self,
        after: Option<(&str, &str)>,
        limit: i64,
    ) -> Result<Vec<SynapseDeviceKeys>, MigrationError> {
        let (user, device) = after.unzip();
        let union = self.e2e_device_union();
        let devices: Vec<(String, String)> = self
            .client
            .query(
                &format!(
                    "SELECT d.user_id, d.device_id FROM ({union}) d JOIN users u ON u.name = d.user_id \
                     WHERE $1::text IS NULL OR (d.user_id, d.device_id) > ($1::text, $2::text) \
                     ORDER BY d.user_id, d.device_id LIMIT $3"
                ),
                &[&user, &device, &limit],
            )
            .await
            .map_err(db)?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        let mut out = Vec::with_capacity(devices.len());
        let mut signatures_of: Option<(String, Vec<(String, rows::HeldSignature)>)> = None;
        for (user_id, device_id) in devices {
            if signatures_of.as_ref().is_none_or(|(u, _)| *u != user_id) {
                signatures_of = Some((user_id.clone(), self.signatures_on(&user_id).await?));
            }
            let on_device: Vec<rows::HeldSignature> = signatures_of
                .as_ref()
                .map(|(_, all)| {
                    all.iter()
                        .filter(|(target, _)| *target == device_id)
                        .map(|(_, held)| held.clone())
                        .collect()
                })
                .unwrap_or_default();
            let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [&user_id, &device_id];
            let keys = self
                .json_rows(
                    "e2e_device_keys_json",
                    "SELECT to_jsonb(k)::text FROM e2e_device_keys_json k \
                     WHERE k.user_id = $1 AND k.device_id = $2",
                    &params,
                )
                .await?;
            let one_time = self
                .json_rows(
                    "e2e_one_time_keys_json",
                    "SELECT to_jsonb(k)::text FROM e2e_one_time_keys_json k \
                     WHERE k.user_id = $1 AND k.device_id = $2",
                    &params,
                )
                .await?;
            let fallback = self
                .json_rows(
                    "e2e_fallback_keys_json",
                    "SELECT to_jsonb(k)::text FROM e2e_fallback_keys_json k \
                     WHERE k.user_id = $1 AND k.device_id = $2",
                    &params,
                )
                .await?;
            out.push(
                rows::device_keys(
                    &user_id,
                    &device_id,
                    keys.first(),
                    &one_time,
                    &fallback,
                    &on_device,
                )
                .map_err(MigrationError::Source)?,
            );
        }
        Ok(out)
    }

    /// Cross-signing keys of the accounts of this server after `after` (a user id) that have
    /// any.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or unreadable key JSON.
    pub async fn cross_signing(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SynapseCrossSigning>, MigrationError> {
        if !self.has("e2e_cross_signing_keys") {
            return Ok(Vec::new());
        }
        let users: Vec<String> = self
            .client
            .query(
                "SELECT DISTINCT k.user_id FROM e2e_cross_signing_keys k JOIN users u ON u.name = k.user_id \
                 WHERE $1::text IS NULL OR k.user_id > $1 ORDER BY k.user_id LIMIT $2",
                &[&after, &limit],
            )
            .await
            .map_err(db)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let mut out = Vec::with_capacity(users.len());
        for user_id in users {
            let keys = self
                .json_rows(
                    "e2e_cross_signing_keys",
                    "SELECT to_jsonb(k)::text FROM e2e_cross_signing_keys k WHERE k.user_id = $1",
                    &[&user_id],
                )
                .await?;
            let signatures = self.signatures_on(&user_id).await?;
            out.push(
                rows::cross_signing(&user_id, &keys, &signatures)
                    .map_err(MigrationError::Source)?,
            );
        }
        Ok(out)
    }

    /// Key backup versions after `after` (`(user id, version)`), deleted ones included.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or unreadable `auth_data`.
    pub async fn backup_versions(
        &self,
        after: Option<(&str, i64)>,
        limit: i64,
    ) -> Result<Vec<SynapseBackupVersion>, MigrationError> {
        let (user, version) = after.unzip();
        self.json_rows(
            "e2e_room_keys_versions",
            "SELECT to_jsonb(v)::text FROM e2e_room_keys_versions v \
             WHERE $1::text IS NULL OR (v.user_id, v.version) > ($1::text, $2::bigint) \
             ORDER BY v.user_id, v.version LIMIT $3",
            &[&user, &version, &limit],
        )
        .await?
        .iter()
        .map(|row| rows::backup_version(row).map_err(MigrationError::Source))
        .collect()
    }

    /// The room keys in one backup version after `after` (`(room id, session id)`).
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or unreadable session data.
    pub async fn backup_keys(
        &self,
        user_id: &str,
        version: u64,
        after: Option<(&str, &str)>,
        limit: i64,
    ) -> Result<Vec<SynapseRoomKey>, MigrationError> {
        let (room, session) = after.unzip();
        let version = i64::try_from(version).unwrap_or(i64::MAX);
        self.json_rows(
            "e2e_room_keys",
            "SELECT to_jsonb(k)::text FROM e2e_room_keys k \
             WHERE k.user_id = $1 AND k.version = $2 \
               AND ($3::text IS NULL OR (k.room_id, k.session_id) > ($3::text, $4::text)) \
             ORDER BY k.room_id, k.session_id LIMIT $5",
            &[&user_id, &version, &room, &session, &limit],
        )
        .await?
        .iter()
        .map(|row| rows::room_key(row).map_err(MigrationError::Source))
        .collect()
    }

    /// How many room keys one backup version holds.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn backup_key_count(
        &self,
        user_id: &str,
        version: u64,
    ) -> Result<u64, MigrationError> {
        if !self.has("e2e_room_keys") {
            return Ok(0);
        }
        let version = i64::try_from(version).unwrap_or(i64::MAX);
        let n: i64 = self
            .client
            .query_one(
                "SELECT count(*) FROM e2e_room_keys WHERE user_id = $1 AND version = $2",
                &[&user_id, &version],
            )
            .await
            .map_err(db)?
            .get(0);
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// The push rules of the accounts after `after` (a user id) that changed any.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn push_rules(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SynapsePushRules>, MigrationError> {
        if !self.has("push_rules") || !self.has("push_rules_enable") {
            return Ok(Vec::new());
        }
        let users: Vec<String> = self
            .client
            .query(
                "SELECT user_name FROM (SELECT user_name FROM push_rules \
                   UNION SELECT user_name FROM push_rules_enable) p \
                 WHERE $1::text IS NULL OR user_name > $1 ORDER BY user_name LIMIT $2",
                &[&after, &limit],
            )
            .await
            .map_err(db)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let mut out = Vec::with_capacity(users.len());
        for user_id in users {
            let rules = self
                .json_rows(
                    "push_rules",
                    "SELECT to_jsonb(r)::text FROM push_rules r WHERE r.user_name = $1",
                    &[&user_id],
                )
                .await?;
            let enabled = self
                .json_rows(
                    "push_rules_enable",
                    "SELECT to_jsonb(r)::text FROM push_rules_enable r WHERE r.user_name = $1 \
                     ORDER BY r.rule_id",
                    &[&user_id],
                )
                .await?;
            out.push(rows::push_rules(&user_id, &rules, &enabled));
        }
        Ok(out)
    }

    /// Pushers after row id `after`.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or unreadable pusher data.
    pub async fn pushers(
        &self,
        after: Option<i64>,
        limit: i64,
    ) -> Result<Vec<SynapsePusher>, MigrationError> {
        self.json_rows(
            "pushers",
            "SELECT to_jsonb(p)::text FROM pushers p WHERE $1::bigint IS NULL OR p.id > $1 \
             ORDER BY p.id LIMIT $2",
            &[&after, &limit],
        )
        .await?
        .iter()
        .map(|row| rows::pusher(row).map_err(MigrationError::Source))
        .collect()
    }

    /// Read receipts after `after` (a stream position), in the order they were sent.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn receipts(
        &self,
        after: Option<i64>,
        limit: i64,
    ) -> Result<Vec<SynapseReceipt>, MigrationError> {
        self.json_rows(
            "receipts_linearized",
            "SELECT to_jsonb(r)::text FROM receipts_linearized r \
             WHERE $1::bigint IS NULL OR r.stream_id > $1 ORDER BY r.stream_id LIMIT $2",
            &[&after, &limit],
        )
        .await?
        .iter()
        .map(|row| rows::receipt(row).map_err(MigrationError::Source))
        .collect()
    }

    /// Sync filters after `after` (`(localpart, filter id)`, the columns Synapse keys them on),
    /// each with the key to carry on after it.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or an unreadable filter.
    pub async fn filters(
        &self,
        after: Option<(&str, i64)>,
        limit: i64,
        server_name: &str,
    ) -> Result<Vec<(SynapseFilter, (String, i64))>, MigrationError> {
        if !self.has("user_filters") {
            return Ok(Vec::new());
        }
        let (user, id) = after.unzip();
        let rows = self
            .client
            .query(
                "SELECT (to_jsonb(f) - 'filter_json')::text, convert_from(f.filter_json, 'UTF8'), \
                   f.user_id, f.filter_id \
                 FROM user_filters f \
                 WHERE $1::text IS NULL OR (f.user_id, f.filter_id) > ($1::text, $2::bigint) \
                 ORDER BY f.user_id, f.filter_id LIMIT $3",
                &[&user, &id, &limit],
            )
            .await
            .map_err(db)?;
        rows.iter()
            .map(|row| {
                let meta = parse_row(row.get(0))?;
                let filter =
                    rows::filter(&meta, row.get(1), server_name).map_err(MigrationError::Source)?;
                Ok((filter, (row.get(2), row.get(3))))
            })
            .collect()
    }
}

/// The checkpoint key of a device row.
#[must_use]
pub fn device_key(device: &SynapseDevice) -> String {
    device_pair_key(&device.user_id, &device.device_id)
}

/// The checkpoint key of a row keyed on a device (a device, a device's end-to-end keys).
#[must_use]
pub fn device_pair_key(user_id: &str, device_id: &str) -> String {
    serde_json::json!([user_id, device_id]).to_string()
}

/// Reads a device checkpoint back.
#[must_use]
pub fn parse_device_key(key: &str) -> Option<(String, String)> {
    serde_json::from_str(key).ok()
}

/// The checkpoint key of an account-data row.
#[must_use]
pub fn account_data_key(key: &[String; 4]) -> String {
    serde_json::json!(key).to_string()
}

/// Reads an account-data checkpoint back.
#[must_use]
pub fn parse_account_data_key(key: &str) -> Option<[String; 4]> {
    serde_json::from_str(key).ok()
}

/// The checkpoint key of a row keyed on a `(text, number)` pair (a backup version, a filter).
#[must_use]
pub fn pair_key(text: &str, number: i64) -> String {
    serde_json::json!([text, number]).to_string()
}

/// Reads a [`pair_key`] back.
#[must_use]
pub fn parse_pair_key(key: &str) -> Option<(String, i64)> {
    serde_json::from_str(key).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_media_file_is_where_synapse_puts_it_and_nothing_escapes_the_store() {
        assert_eq!(
            local_content_path("QRfDgyLujIkTamUPGRmOJeRy").unwrap(),
            Path::new("local_content/QR/fD/gyLujIkTamUPGRmOJeRy")
        );
        assert_eq!(local_content_path("../../etc/passwd"), None);
        assert_eq!(local_content_path("ab"), None);
    }

    #[test]
    fn a_remote_media_file_is_under_its_server_and_nothing_escapes_the_store() {
        assert_eq!(
            remote_content_path("other.test", "QRfDgyLujIkTamUPGRmOJeRy").unwrap(),
            Path::new("remote_content/other.test/QR/fD/gyLujIkTamUPGRmOJeRy")
        );
        assert_eq!(
            remote_content_path("127.0.0.1:18302", "abcdefgh").unwrap(),
            Path::new("remote_content/127.0.0.1:18302/ab/cd/efgh")
        );
        assert_eq!(remote_content_path("..", "QRfDgyLujIkTamUPGRmOJeRy"), None);
        assert_eq!(remote_content_path("a/b", "QRfDgyLujIkTamUPGRmOJeRy"), None);
        assert_eq!(remote_content_path("", "QRfDgyLujIkTamUPGRmOJeRy"), None);
        assert_eq!(remote_content_path("other.test", "../../etc/passwd"), None);
    }

    #[test]
    fn checkpoints_read_back_as_written() {
        let device = SynapseDevice {
            user_id: "@a:x".into(),
            device_id: "D\"1".into(),
            display_name: None,
            last_seen_ms: None,
            last_seen_ip: None,
            hidden: false,
        };
        assert_eq!(
            parse_device_key(&device_key(&device)),
            Some(("@a:x".into(), "D\"1".into()))
        );
        let key = [
            "g".to_owned(),
            "@a:x".to_owned(),
            String::new(),
            "m.direct".to_owned(),
        ];
        assert_eq!(parse_account_data_key(&account_data_key(&key)), Some(key));
    }
}
