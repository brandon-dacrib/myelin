//! Reading a Synapse database: [`SynapseSource`].
//!
//! Every query is a read, keyed and paged on a column that orders rows stably (a user's name,
//! a token's id), so a copy can stop between any two batches and pick up after the last row it
//! handled. Rows are read as `to_jsonb(row)`, so a column an older or newer Synapse lacks or adds
//! is simply absent or ignored rather than an error; the table and column names are those in
//! `docs/compat/synapse-importer-mapping.md`. Nothing is ever written to Synapse's database.

use std::path::{Path, PathBuf};

use hs_config::migration::SynapseSourceConfig;
use serde_json::Value;
use tokio_postgres::{Client, NoTls};

use super::MigrationError;
use super::model::{
    Stream, SynapseAccessToken, SynapseAccountData, SynapseDevice, SynapseEvent, SynapseMedia,
    SynapseRoom, SynapseUser,
};

/// A connection to a Synapse deployment: its database, and its media store if mounted.
pub struct SynapseSource {
    client: Client,
    media_store: Option<PathBuf>,
    description: String,
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
        let source = Self {
            client,
            media_store: config.media_store_path.clone(),
            description,
        };
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
        let sql = match stream {
            Stream::Users => "SELECT count(*) FROM users",
            Stream::Devices => "SELECT count(*) FROM devices",
            Stream::AccessTokens => "SELECT count(*) FROM access_tokens",
            Stream::AccountData => {
                "SELECT (SELECT count(*) FROM account_data) + (SELECT count(*) FROM room_account_data) \
                 + (SELECT count(*) FROM (SELECT DISTINCT user_id, room_id FROM room_tags) t)"
            }
            Stream::Rooms => "SELECT count(*) FROM rooms",
            Stream::Media => "SELECT count(*) FROM local_media_repository",
        };
        let n: i64 = self.client.query_one(sql, &[]).await.map_err(db)?.get(0);
        Ok(u64::try_from(n).unwrap_or(0))
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

    /// One room, with all its events, aliases and redactions.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error or an event whose JSON is unreadable.
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
        let event_rows = self
            .client
            .query(
                "SELECT e.event_id, e.depth, coalesce(e.stream_ordering, 0), e.outlier, \
                   (to_jsonb(e)->>'rejection_reason') IS NOT NULL \
                     OR EXISTS (SELECT 1 FROM rejections x WHERE x.event_id = e.event_id), \
                   j.json \
                 FROM events e JOIN event_json j ON j.event_id = e.event_id \
                 WHERE e.room_id = $1 ORDER BY e.stream_ordering, e.event_id",
                &[&room_id],
            )
            .await
            .map_err(db)?;
        let mut events = Vec::with_capacity(event_rows.len());
        for row in &event_rows {
            let event_id: String = row.get(0);
            let raw: String = row.get(5);
            let mut json: Value = serde_json::from_str(&raw).map_err(|e| {
                MigrationError::Source(format!("event {event_id} holds unreadable JSON: {e}"))
            })?;
            if let Some(object) = json.as_object_mut() {
                object.remove("unsigned");
            }
            events.push(SynapseEvent {
                event_id,
                json,
                depth: row.get(1),
                stream_ordering: row.get(2),
                outlier: row.get(3),
                rejected: row.get(4),
            });
        }
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
            events,
            redactions,
        }))
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

    /// The ids of every row of `stream` that is meant to be copied, for verification's counts.
    /// Rooms are counted by room; events are compared room by room.
    ///
    /// # Errors
    /// [`MigrationError::Source`] on a database error.
    pub async fn sample_keys(
        &self,
        stream: Stream,
        limit: i64,
    ) -> Result<Vec<String>, MigrationError> {
        let sql = match stream {
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
}

/// The checkpoint key of a device row.
#[must_use]
pub fn device_key(device: &SynapseDevice) -> String {
    serde_json::json!([device.user_id, device.device_id]).to_string()
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
