//! The migration engine against a real Synapse database (`tests/fixtures/synapse-small`, see its
//! README), with this server's stores stood in for by an in-memory target: the reader, the
//! copy, pausing and resuming, abandoning, carrying on after a restart, verification that
//! notices a difference, and a cutover. And a room's copy a page at a time, with events out of
//! order, against no database at all.
//!
//! `crates/hs-cli/tests/migration.rs` runs the same fixture through the real `hs` binary.
//!
//! Needs PostgreSQL (`HS_MIGRATION_TEST_POSTGRES_DSN`, or the local
//! `postgres://postgres:hspg@127.0.0.1:5439/postgres`); every test that reads the fixture skips,
//! saying so, without one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hs_admin::migration::{MigrationSource, MigrationStartRequest};
use hs_admin::model::Actor;
use hs_admin::sources::SourceError;
use hs_admin::tasks::TaskRegistry;
use hs_compat::migration::engine::MigratorParts;
use hs_compat::migration::model::{
    MigrationRecord, Phase, Stream, SynapseAccessToken, SynapseAccountData, SynapseBackupVersion,
    SynapseCrossSigning, SynapseDevice, SynapseDeviceKeys, SynapseEvent, SynapseExternalId,
    SynapseFilter, SynapseMedia, SynapsePushRules, SynapsePusher, SynapseReceipt,
    SynapseRefreshToken, SynapseRegistrationToken, SynapseRemoteJoin, SynapseRemoteMedia,
    SynapseRoom, SynapseRoomKey, SynapseThreepid, SynapseToDeviceMessage, SynapseUser,
};
use hs_compat::migration::rooms::{EventPages, copy_room};
use hs_compat::migration::source::EventKey;
use hs_compat::migration::{
    Check, CurrentState, Imported, InMemoryMigrationStore, MigrationError, MigrationStore,
    MigrationTarget, Migrator, RoomOutcome, SourceConfigs, SynapseSource, TargetError, TargetMedia,
    TargetUser,
};
use hs_config::migration::{SynapseDatabaseConfig, SynapseSourceConfig};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/synapse-small")
}

/// A fresh database holding the fixture, dropped when the test ends.
struct Fixture {
    admin_dsn: String,
    name: String,
    config: SynapseSourceConfig,
}

impl Fixture {
    async fn load() -> Option<Self> {
        Self::load_from(fixture_dir()).await
    }

    /// `tests/fixtures/synapse-federated`: a Synapse that joined two rooms of another server.
    async fn federated() -> Option<Self> {
        Self::load_from(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/synapse-federated"),
        )
        .await
    }

    async fn load_from(dir: PathBuf) -> Option<Self> {
        let admin_dsn = std::env::var("HS_MIGRATION_TEST_POSTGRES_DSN")
            .unwrap_or_else(|_| "postgres://postgres:hspg@127.0.0.1:5439/postgres".to_owned());
        let parsed: tokio_postgres::Config = admin_dsn.parse().ok()?;
        let (admin, connection) = match parsed.connect(tokio_postgres::NoTls).await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!(
                    "SKIP: the migration tests need PostgreSQL at {admin_dsn:?}: {e}. Start one \
                     with: docker run --rm -d -e POSTGRES_PASSWORD=hspg -p 127.0.0.1:5439:5432 \
                     postgres:17"
                );
                return None;
            }
        };
        tokio::spawn(connection);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("synapse_small_{}_{nanos}_{n}", std::process::id());
        admin
            .batch_execute(&format!(
                "CREATE DATABASE {name} ENCODING 'UTF8' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0"
            ))
            .await
            .unwrap();
        let mut db = parsed.clone();
        db.dbname(&name);
        let (client, connection) = db.connect(tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        for file in ["schema.sql", "data.sql"] {
            let sql = std::fs::read_to_string(dir.join(file)).unwrap();
            client.batch_execute(&sql).await.unwrap();
        }
        let host = match parsed.get_hosts().first() {
            Some(tokio_postgres::config::Host::Tcp(host)) => host.clone(),
            _ => "127.0.0.1".to_owned(),
        };
        let config = SynapseSourceConfig {
            database: SynapseDatabaseConfig {
                host,
                port: parsed.get_ports().first().copied().unwrap_or(5432),
                database: name.clone(),
                user: parsed.get_user().unwrap_or("postgres").to_owned(),
                password: String::from_utf8_lossy(parsed.get_password().unwrap_or_default())
                    .into_owned()
                    .into(),
            },
            media_store_path: Some(dir.join("media_store")),
            batch_size: 2,
        };
        Some(Self {
            admin_dsn,
            name,
            config,
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let dsn = self.admin_dsn.clone();
        let name = self.name.clone();
        // Dropped from inside a runtime: do it on a thread of its own.
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                if let Ok((client, connection)) = dsn
                    .parse::<tokio_postgres::Config>()
                    .unwrap()
                    .connect(tokio_postgres::NoTls)
                    .await
                {
                    tokio::spawn(connection);
                    let _ = client
                        .batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                        .await;
                }
            });
        })
        .join();
    }
}

struct Configs(Option<SynapseSourceConfig>);

#[async_trait]
impl SourceConfigs for Configs {
    async fn source(&self, pointer: &str) -> Result<Option<SynapseSourceConfig>, String> {
        Ok(if pointer == "/migration/synapse" {
            self.0.clone()
        } else {
            None
        })
    }
}

/// `(user, room, type)`.
type AccountDataKey = (String, Option<String>, String);

/// A backup version and its room keys by session, under `(user, version)`.
type Backups = HashMap<(String, u64), (SynapseBackupVersion, HashMap<String, SynapseRoomKey>)>;

/// `(room, user, receipt type, thread) -> (event, ts)`.
type Receipts = HashMap<(String, String, String, Option<String>), (String, u64)>;

/// A room as the in-memory target holds it.
#[derive(Default, Clone)]
struct MemoryRoom {
    event_ids: HashSet<String>,
    state: CurrentState,
    finished: bool,
}

/// This server's stores, in memory. Accounts can be held up (`gate`) to catch a copy part way.
#[derive(Default)]
struct MemoryTarget {
    /// The server name it answers; `fixture.test` when unset.
    name: Option<String>,
    users: Mutex<HashMap<String, TargetUser>>,
    devices: Mutex<HashMap<(String, String), Option<String>>>,
    tokens: Mutex<HashMap<String, (String, Option<String>)>>,
    refresh_tokens: Mutex<HashMap<String, SynapseRefreshToken>>,
    threepids: Mutex<HashMap<(String, String), String>>,
    external_ids: Mutex<HashMap<(String, String), String>>,
    to_device: Mutex<Vec<SynapseToDeviceMessage>>,
    registration_tokens: Mutex<HashMap<String, SynapseRegistrationToken>>,
    account_data: Mutex<HashMap<AccountDataKey, Value>>,
    device_keys: Mutex<HashMap<(String, String), SynapseDeviceKeys>>,
    cross_signing: Mutex<HashMap<String, SynapseCrossSigning>>,
    backups: Mutex<Backups>,
    push_rules: Mutex<HashMap<String, SynapsePushRules>>,
    pushers: Mutex<HashMap<(String, String, String), SynapsePusher>>,
    filters: Mutex<HashMap<(String, String), Value>>,
    receipts: Mutex<Receipts>,
    rooms: Mutex<HashMap<String, MemoryRoom>>,
    media: Mutex<HashMap<String, TargetMedia>>,
    remote_media: Mutex<HashMap<(String, String), TargetMedia>>,
    /// The most events any one call to `import_room_events` was given.
    largest_page: AtomicUsize,
    gate: Option<Arc<Semaphore>>,
}

/// What an event cites: its `prev_events` and `auth_events`.
fn cited(event: &SynapseEvent) -> Vec<String> {
    let mut ids = Vec::new();
    for key in ["prev_events", "auth_events"] {
        for item in event.json[key].as_array().into_iter().flatten() {
            match item {
                Value::String(id) => ids.push(id.clone()),
                Value::Array(pair) => {
                    if let Some(Value::String(id)) = pair.first() {
                        ids.push(id.clone());
                    }
                }
                _ => {}
            }
        }
    }
    ids
}

fn upsert<K: std::hash::Hash + Eq, V: PartialEq>(
    map: &Mutex<HashMap<K, V>>,
    key: K,
    value: V,
) -> Imported {
    match map.lock().unwrap().insert(key, value) {
        None => Imported::Created,
        Some(_) => Imported::Updated,
    }
}

fn check<V: PartialEq>(here: Option<&V>, theirs: &V) -> Check {
    match here {
        None => Check::Missing,
        Some(here) if here == theirs => Check::Same,
        Some(_) => Check::Differs("differs".to_owned()),
    }
}

#[async_trait]
impl MigrationTarget for MemoryTarget {
    fn server_name(&self) -> &str {
        self.name.as_deref().unwrap_or("fixture.test")
    }

    async fn import_user(&self, user: &SynapseUser) -> Result<Imported, TargetError> {
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        let ours = TargetUser {
            password_hash: user.password_hash.clone(),
            displayname: user.displayname.clone(),
            avatar_url: user.avatar_url.clone(),
            admin: user.admin,
            deactivated: user.deactivated,
            erased: user.erased,
        };
        let mut users = self.users.lock().unwrap();
        Ok(match users.insert(user.user_id.clone(), ours.clone()) {
            None => Imported::Created,
            Some(before) if before == ours => Imported::AlreadyThere,
            Some(_) => Imported::Updated,
        })
    }

    async fn import_device(&self, device: &SynapseDevice) -> Result<Imported, TargetError> {
        let key = (device.user_id.clone(), device.device_id.clone());
        let before = self
            .devices
            .lock()
            .unwrap()
            .insert(key, device.display_name.clone());
        Ok(if before.is_some() {
            Imported::AlreadyThere
        } else {
            Imported::Created
        })
    }

    async fn import_access_token(
        &self,
        token: &SynapseAccessToken,
    ) -> Result<Imported, TargetError> {
        let before = self.tokens.lock().unwrap().insert(
            token.token.clone(),
            (token.user_id.clone(), token.device_id.clone()),
        );
        Ok(if before.is_some() {
            Imported::AlreadyThere
        } else {
            Imported::Created
        })
    }

    async fn import_refresh_token(
        &self,
        token: &SynapseRefreshToken,
    ) -> Result<Imported, TargetError> {
        let mut held = self.refresh_tokens.lock().unwrap();
        Ok(match held.insert(token.token.clone(), token.clone()) {
            None => Imported::Created,
            Some(before) if before == *token => Imported::AlreadyThere,
            Some(_) => Imported::Updated,
        })
    }

    async fn import_threepid(&self, threepid: &SynapseThreepid) -> Result<Imported, TargetError> {
        let key = (threepid.medium.clone(), threepid.address.clone());
        let mut held = self.threepids.lock().unwrap();
        Ok(match held.get(&key) {
            Some(user) if *user == threepid.user_id => Imported::AlreadyThere,
            Some(_) => return Err(TargetError::row("bound to another account here")),
            None => {
                held.insert(key, threepid.user_id.clone());
                Imported::Created
            }
        })
    }

    async fn import_external_id(&self, link: &SynapseExternalId) -> Result<Imported, TargetError> {
        let key = (link.provider.clone(), link.external_id.clone());
        let mut held = self.external_ids.lock().unwrap();
        Ok(match held.get(&key) {
            Some(user) if *user == link.user_id => Imported::AlreadyThere,
            Some(_) => return Err(TargetError::row("linked to another account here")),
            None => {
                held.insert(key, link.user_id.clone());
                Imported::Created
            }
        })
    }

    async fn import_to_device(
        &self,
        message: &SynapseToDeviceMessage,
    ) -> Result<Imported, TargetError> {
        if !self
            .devices
            .lock()
            .unwrap()
            .contains_key(&(message.user_id.clone(), message.device_id.clone()))
        {
            return Ok(Imported::Skipped("its device was not copied".to_owned()));
        }
        let mut queue = self.to_device.lock().unwrap();
        if queue.iter().any(|m| {
            m.user_id == message.user_id
                && m.device_id == message.device_id
                && m.sender == message.sender
                && m.event_type == message.event_type
                && m.content == message.content
        }) {
            return Ok(Imported::AlreadyThere);
        }
        queue.push(message.clone());
        Ok(Imported::Created)
    }

    async fn import_registration_token(
        &self,
        token: &SynapseRegistrationToken,
    ) -> Result<Imported, TargetError> {
        let mut held = self.registration_tokens.lock().unwrap();
        Ok(match held.insert(token.token.clone(), token.clone()) {
            None => Imported::Created,
            Some(before) if before == *token => Imported::AlreadyThere,
            Some(_) => Imported::Updated,
        })
    }

    async fn import_account_data(
        &self,
        data: &SynapseAccountData,
    ) -> Result<Imported, TargetError> {
        self.account_data.lock().unwrap().insert(
            (
                data.user_id.clone(),
                data.room_id.clone(),
                data.data_type.clone(),
            ),
            data.content.clone(),
        );
        Ok(Imported::Created)
    }

    async fn import_device_keys(&self, keys: &SynapseDeviceKeys) -> Result<Imported, TargetError> {
        Ok(upsert(
            &self.device_keys,
            (keys.user_id.clone(), keys.device_id.clone()),
            keys.clone(),
        ))
    }

    async fn import_cross_signing(
        &self,
        keys: &SynapseCrossSigning,
    ) -> Result<Imported, TargetError> {
        Ok(upsert(
            &self.cross_signing,
            keys.user_id.clone(),
            keys.clone(),
        ))
    }

    async fn import_backup_version(
        &self,
        version: &SynapseBackupVersion,
    ) -> Result<Imported, TargetError> {
        let mut backups = self.backups.lock().unwrap();
        let entry = backups
            .entry((version.user_id.clone(), version.version))
            .or_insert_with(|| (version.clone(), HashMap::new()));
        entry.0 = version.clone();
        Ok(Imported::Created)
    }

    async fn import_backup_keys(
        &self,
        user_id: &str,
        version: u64,
        keys: &[SynapseRoomKey],
    ) -> Result<u64, TargetError> {
        let mut backups = self.backups.lock().unwrap();
        let (_, held) = backups
            .get_mut(&(user_id.to_owned(), version))
            .ok_or_else(|| TargetError::row("no such version"))?;
        let mut stored = 0;
        for key in keys {
            if held.insert(key.session_id.clone(), key.clone()).as_ref() != Some(key) {
                stored += 1;
            }
        }
        Ok(stored)
    }

    async fn import_push_rules(&self, rules: &SynapsePushRules) -> Result<Imported, TargetError> {
        Ok(upsert(
            &self.push_rules,
            rules.user_id.clone(),
            rules.clone(),
        ))
    }

    async fn import_pusher(&self, pusher: &SynapsePusher) -> Result<Imported, TargetError> {
        Ok(upsert(
            &self.pushers,
            (
                pusher.user_id.clone(),
                pusher.app_id.clone(),
                pusher.pushkey.clone(),
            ),
            pusher.clone(),
        ))
    }

    async fn import_filter(&self, filter: &SynapseFilter) -> Result<Imported, TargetError> {
        Ok(upsert(
            &self.filters,
            (filter.user_id.clone(), filter.filter_id.clone()),
            filter.filter.clone(),
        ))
    }

    async fn import_remote_join(
        &self,
        room: &SynapseRoom,
        join: &SynapseRemoteJoin,
    ) -> Result<Imported, TargetError> {
        let mut rooms = self.rooms.lock().unwrap();
        let held = rooms.entry(room.room_id.clone()).or_default();
        if held.event_ids.contains(&join.join.event_id) {
            return Ok(Imported::AlreadyThere);
        }
        for event in join.auth_chain.iter().chain(&join.state) {
            held.event_ids.insert(event.event_id.clone());
        }
        for event in join.state.iter().chain(std::iter::once(&join.join)) {
            if let Some(key) = event.json.get("state_key").and_then(Value::as_str) {
                let kind = event.json["type"].as_str().unwrap_or_default().to_owned();
                held.state
                    .insert((kind, key.to_owned()), event.event_id.clone());
            }
        }
        held.event_ids.insert(join.join.event_id.clone());
        Ok(Imported::Created)
    }

    async fn begin_room(&self, room: &SynapseRoom) -> Result<(), TargetError> {
        self.rooms
            .lock()
            .unwrap()
            .entry(room.room_id.clone())
            .or_default();
        Ok(())
    }

    async fn import_room_events(
        &self,
        room: &SynapseRoom,
        events: &[SynapseEvent],
    ) -> Result<RoomOutcome, TargetError> {
        self.largest_page.fetch_max(events.len(), Ordering::Relaxed);
        let mut rooms = self.rooms.lock().unwrap();
        let held = rooms.entry(room.room_id.clone()).or_default();
        let mut outcome = RoomOutcome::default();
        for event in events {
            if held.event_ids.contains(&event.event_id) {
                outcome.already_there += 1;
                continue;
            }
            if cited(event).iter().any(|id| !held.event_ids.contains(id)) {
                outcome.waiting.push(event.event_id.clone());
                continue;
            }
            held.event_ids.insert(event.event_id.clone());
            outcome.stored += 1;
            if let Some(key) = event.json.get("state_key").and_then(Value::as_str) {
                let kind = event.json["type"].as_str().unwrap_or_default().to_owned();
                held.state
                    .insert((kind, key.to_owned()), event.event_id.clone());
            }
        }
        Ok(outcome)
    }

    async fn finish_room(&self, room: &SynapseRoom) -> Result<RoomOutcome, TargetError> {
        let mut rooms = self.rooms.lock().unwrap();
        let held = rooms.entry(room.room_id.clone()).or_default();
        if held.event_ids.is_empty() {
            rooms.remove(&room.room_id);
            return Err(TargetError::row("none of its events could be stored"));
        }
        held.finished = true;
        Ok(RoomOutcome {
            redactions: room.redactions.len() as u64,
            aliases: room.aliases.len() as u64,
            ..RoomOutcome::default()
        })
    }

    async fn import_receipt(&self, receipt: &SynapseReceipt) -> Result<Imported, TargetError> {
        if !self.rooms.lock().unwrap().contains_key(&receipt.room_id) {
            return Ok(Imported::Skipped("its room was not copied".to_owned()));
        }
        Ok(upsert(
            &self.receipts,
            (
                receipt.room_id.clone(),
                receipt.user_id.clone(),
                receipt.receipt_type.clone(),
                receipt.thread_id.clone(),
            ),
            (receipt.event_id.clone(), receipt.ts),
        ))
    }

    async fn import_media(
        &self,
        media: &SynapseMedia,
        bytes: Option<Vec<u8>>,
    ) -> Result<Imported, TargetError> {
        let Some(bytes) = bytes else {
            return Ok(Imported::Skipped("no file".to_owned()));
        };
        self.media.lock().unwrap().insert(
            media.media_id.clone(),
            TargetMedia {
                content_type: media.content_type.clone().unwrap_or_default(),
                bytes: Some(bytes),
            },
        );
        Ok(Imported::Created)
    }

    async fn import_remote_media(
        &self,
        media: &SynapseRemoteMedia,
        bytes: Vec<u8>,
    ) -> Result<Imported, TargetError> {
        Ok(upsert(
            &self.remote_media,
            (media.origin.clone(), media.media_id.clone()),
            TargetMedia {
                content_type: media.content_type.clone().unwrap_or_default(),
                bytes: Some(bytes),
            },
        ))
    }

    async fn user(&self, user_id: &str) -> Result<Option<TargetUser>, TargetError> {
        Ok(self.users.lock().unwrap().get(user_id).cloned())
    }

    async fn device(
        &self,
        user_id: &str,
        device_id: &str,
    ) -> Result<Option<Option<String>>, TargetError> {
        Ok(self
            .devices
            .lock()
            .unwrap()
            .get(&(user_id.to_owned(), device_id.to_owned()))
            .cloned())
    }

    async fn access_token(
        &self,
        token: &str,
    ) -> Result<Option<(String, Option<String>)>, TargetError> {
        Ok(self.tokens.lock().unwrap().get(token).cloned())
    }

    async fn account_data(
        &self,
        user_id: &str,
        room_id: Option<&str>,
        data_type: &str,
    ) -> Result<Option<Value>, TargetError> {
        Ok(self
            .account_data
            .lock()
            .unwrap()
            .get(&(
                user_id.to_owned(),
                room_id.map(str::to_owned),
                data_type.to_owned(),
            ))
            .cloned())
    }

    async fn room_state(&self, room_id: &str) -> Result<Option<CurrentState>, TargetError> {
        Ok(self
            .rooms
            .lock()
            .unwrap()
            .get(room_id)
            .map(|r| r.state.clone()))
    }

    async fn missing_events(
        &self,
        room_id: &str,
        event_ids: &[String],
    ) -> Result<Vec<String>, TargetError> {
        let rooms = self.rooms.lock().unwrap();
        let held = rooms.get(room_id).map(|r| &r.event_ids);
        Ok(event_ids
            .iter()
            .filter(|id| !held.is_some_and(|h| h.contains(*id)))
            .cloned()
            .collect())
    }

    async fn media(&self, media_id: &str) -> Result<Option<TargetMedia>, TargetError> {
        Ok(self.media.lock().unwrap().get(media_id).cloned())
    }

    async fn remote_media(
        &self,
        origin: &str,
        media_id: &str,
    ) -> Result<Option<TargetMedia>, TargetError> {
        Ok(self
            .remote_media
            .lock()
            .unwrap()
            .get(&(origin.to_owned(), media_id.to_owned()))
            .cloned())
    }

    async fn verify_refresh_token(
        &self,
        token: &SynapseRefreshToken,
    ) -> Result<Check, TargetError> {
        Ok(check(
            self.refresh_tokens.lock().unwrap().get(&token.token),
            token,
        ))
    }

    async fn verify_threepid(&self, threepid: &SynapseThreepid) -> Result<Check, TargetError> {
        Ok(check(
            self.threepids
                .lock()
                .unwrap()
                .get(&(threepid.medium.clone(), threepid.address.clone())),
            &threepid.user_id,
        ))
    }

    async fn verify_external_id(&self, link: &SynapseExternalId) -> Result<Check, TargetError> {
        Ok(check(
            self.external_ids
                .lock()
                .unwrap()
                .get(&(link.provider.clone(), link.external_id.clone())),
            &link.user_id,
        ))
    }

    async fn verify_to_device(
        &self,
        message: &SynapseToDeviceMessage,
    ) -> Result<Check, TargetError> {
        let queue = self.to_device.lock().unwrap();
        Ok(
            if queue.iter().any(|m| {
                m.user_id == message.user_id
                    && m.device_id == message.device_id
                    && m.sender == message.sender
                    && m.event_type == message.event_type
                    && m.content == message.content
            }) {
                Check::Same
            } else {
                Check::Missing
            },
        )
    }

    async fn verify_registration_token(
        &self,
        token: &SynapseRegistrationToken,
    ) -> Result<Check, TargetError> {
        Ok(check(
            self.registration_tokens.lock().unwrap().get(&token.token),
            token,
        ))
    }

    async fn verify_device_keys(&self, keys: &SynapseDeviceKeys) -> Result<Check, TargetError> {
        Ok(check(
            self.device_keys
                .lock()
                .unwrap()
                .get(&(keys.user_id.clone(), keys.device_id.clone())),
            keys,
        ))
    }

    async fn verify_cross_signing(&self, keys: &SynapseCrossSigning) -> Result<Check, TargetError> {
        Ok(check(
            self.cross_signing.lock().unwrap().get(&keys.user_id),
            keys,
        ))
    }

    async fn verify_backup_version(
        &self,
        version: &SynapseBackupVersion,
        key_count: u64,
    ) -> Result<Check, TargetError> {
        let backups = self.backups.lock().unwrap();
        Ok(
            match backups.get(&(version.user_id.clone(), version.version)) {
                None => Check::Missing,
                Some((here, keys)) if here == version && keys.len() as u64 == key_count => {
                    Check::Same
                }
                Some(_) => Check::Differs("differs".to_owned()),
            },
        )
    }

    async fn verify_push_rules(&self, rules: &SynapsePushRules) -> Result<Check, TargetError> {
        Ok(check(
            self.push_rules.lock().unwrap().get(&rules.user_id),
            rules,
        ))
    }

    async fn verify_pusher(&self, pusher: &SynapsePusher) -> Result<Check, TargetError> {
        Ok(check(
            self.pushers.lock().unwrap().get(&(
                pusher.user_id.clone(),
                pusher.app_id.clone(),
                pusher.pushkey.clone(),
            )),
            pusher,
        ))
    }

    async fn verify_filter(&self, filter: &SynapseFilter) -> Result<Check, TargetError> {
        Ok(check(
            self.filters
                .lock()
                .unwrap()
                .get(&(filter.user_id.clone(), filter.filter_id.clone())),
            &filter.filter,
        ))
    }

    async fn verify_receipt(&self, receipt: &SynapseReceipt) -> Result<Check, TargetError> {
        Ok(check(
            self.receipts.lock().unwrap().get(&(
                receipt.room_id.clone(),
                receipt.user_id.clone(),
                receipt.receipt_type.clone(),
                receipt.thread_id.clone(),
            )),
            &(receipt.event_id.clone(), receipt.ts),
        ))
    }
}

fn operator() -> Actor {
    let mut actor = Actor::system();
    actor.id = "@ops:fixture.test".to_owned();
    actor
}

struct Rig {
    migrator: Arc<Migrator>,
    store: Arc<InMemoryMigrationStore>,
    target: Arc<MemoryTarget>,
    tasks: Arc<TaskRegistry>,
}

fn rig(
    config: Option<SynapseSourceConfig>,
    target: MemoryTarget,
    store: Option<Arc<InMemoryMigrationStore>>,
) -> Rig {
    let store = store.unwrap_or_else(|| Arc::new(InMemoryMigrationStore::new()));
    let target = Arc::new(target);
    let tasks = TaskRegistry::in_memory();
    let migrator = Migrator::new(MigratorParts {
        store: store.clone(),
        target: target.clone(),
        configs: Arc::new(Configs(config)),
        tasks: tasks.clone(),
        events: None,
        observer: None,
        sample_size: 25,
    });
    Rig {
        migrator,
        store,
        target,
        tasks,
    }
}

async fn reaches(store: &InMemoryMigrationStore, phase: Phase) -> MigrationRecord {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let record = store.load().await.unwrap();
        if record.phase == phase {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "never reached {phase:?}: {record:#?}\n{:#?}",
            store.log().await.unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn copied(record: &MigrationRecord, stream: Stream) -> u64 {
    record.stream(stream).map_or(0, |s| s.copied)
}

fn facts() -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture_dir().join("facts.json")).unwrap())
        .unwrap()
}

#[tokio::test]
async fn the_reader_sees_what_synapse_holds() {
    let Some(fixture) = Fixture::load().await else {
        return;
    };
    let facts = facts();
    let source = SynapseSource::connect(&fixture.config).await.unwrap();
    assert_eq!(
        source.server_name().await.unwrap().as_deref(),
        Some("fixture.test")
    );
    let users = source.users(None, 100).await.unwrap();
    assert_eq!(users.len(), 4);
    let alice = users
        .iter()
        .find(|u| u.user_id == "@alice:fixture.test")
        .unwrap();
    assert!(alice.admin);
    assert!(
        alice
            .password_hash
            .as_deref()
            .is_some_and(|h| h.starts_with("$2b$"))
    );
    assert_eq!(alice.displayname.as_deref(), Some("Alice Liddell"));
    assert!(alice.created_at_ms > 1_000_000_000_000);
    let dave = users
        .iter()
        .find(|u| u.user_id == "@dave:fixture.test")
        .unwrap();
    assert!(dave.deactivated);

    // Keyset paging reads each row once.
    let first = source.users(None, 3).await.unwrap();
    let rest = source
        .users(Some(&first.last().unwrap().user_id), 3)
        .await
        .unwrap();
    assert_eq!(first.len() + rest.len(), 4);

    // A room's history, a page at a time, the create event first, every page after the last.
    for room_id in source.room_ids(None, 10).await.unwrap() {
        let room = source.room(&room_id).await.unwrap().unwrap();
        assert_eq!(room.room_version, "11");
        let shape = source.room_shape(&room_id).await.unwrap();
        assert!(shape.has_create, "{room_id}");
        assert_eq!(shape.outliers + shape.rejected, 0, "{room_id}");
        let mut after = None;
        let mut seen: Vec<SynapseEvent> = Vec::new();
        loop {
            let page = source.room_events(&room_id, after, 3, None).await.unwrap();
            assert!(page.len() <= 3);
            let Some((_, last)) = page.last() else { break };
            after = Some(*last);
            seen.extend(page.into_iter().map(|(e, _)| e));
        }
        assert_eq!(seen.len() as u64, shape.history, "{room_id}");
        assert_eq!(seen[0].json["type"], "m.room.create");
        // Each event comes after every event it cites.
        let mut before = HashSet::new();
        for event in &seen {
            for id in cited(event) {
                assert!(
                    before.contains(&id),
                    "{} cites {id} before it",
                    event.event_id
                );
            }
            before.insert(event.event_id.clone());
        }
        assert!(
            seen.iter()
                .all(|e| e.json_bytes > 0 && e.json.get("unsigned").is_none())
        );
    }
    let data = source.account_data(None, 100).await.unwrap();
    assert!(
        data.iter()
            .any(|(d, _)| d.data_type == "m.tag" && d.content["tags"]["m.favourite"].is_object()),
        "{data:?}"
    );
    let media = source.media(None, 10).await.unwrap();
    assert_eq!(media.len(), 2);
    for (item, _) in &media {
        let bytes = source.media_bytes(&item.media_id).await.unwrap().unwrap();
        assert_eq!(Some(bytes.len() as u64), item.length);
    }

    // End-to-end keys: alice's phone with her self-signing key's signature merged in, its five
    // one-time keys and its fallback key; bob's laptop.
    let devices = source.e2e_device_keys(None, 10).await.unwrap();
    let ids: Vec<(&str, &str)> = devices
        .iter()
        .map(|d| (d.user_id.as_str(), d.device_id.as_str()))
        .collect();
    assert_eq!(
        ids,
        [
            ("@alice:fixture.test", "ALICEPHONE"),
            ("@bob:fixture.test", "BOBLAPTOP")
        ]
    );
    let phone = &devices[0];
    let ssk = format!(
        "ed25519:{}",
        facts["alice_self_signing_key"].as_str().unwrap()
    );
    let signatures = &phone.keys.as_ref().unwrap()["signatures"]["@alice:fixture.test"];
    assert!(
        signatures.get("ed25519:ALICEPHONE").is_some(),
        "{signatures}"
    );
    assert!(signatures.get(&ssk).is_some(), "{signatures}");
    assert_eq!(phone.one_time_keys.len(), 5);
    assert_eq!(phone.fallback_keys.len(), 1);
    assert_eq!(devices[1].one_time_keys.len(), 3);
    // Paging carries on after a device.
    let rest = source
        .e2e_device_keys(Some(("@alice:fixture.test", "ALICEPHONE")), 10)
        .await
        .unwrap();
    assert_eq!(rest.len(), 1);

    // Cross-signing: bob's master key carries alice's signature (she verified him).
    let cross = source.cross_signing(None, 10).await.unwrap();
    assert_eq!(cross.len(), 2);
    let bob = cross
        .iter()
        .find(|c| c.user_id == "@bob:fixture.test")
        .unwrap();
    assert!(bob.self_signing.is_some() && bob.user_signing.is_some());
    assert!(
        bob.master.as_ref().unwrap()["signatures"]["@alice:fixture.test"]
            .as_object()
            .is_some_and(|s| s.len() == 1),
        "{bob:?}"
    );

    // Key backups: version 1 deleted, version 2 with three room keys.
    let versions = source.backup_versions(None, 10).await.unwrap();
    assert_eq!(
        versions
            .iter()
            .map(|v| (v.version, v.deleted))
            .collect::<Vec<_>>(),
        [(1, true), (2, false)]
    );
    assert_eq!(
        source
            .backup_key_count("@alice:fixture.test", 2)
            .await
            .unwrap(),
        3
    );
    let first = source
        .backup_keys("@alice:fixture.test", 2, None, 2)
        .await
        .unwrap();
    let last = first.last().unwrap();
    let more = source
        .backup_keys(
            "@alice:fixture.test",
            2,
            Some((&last.room_id, &last.session_id)),
            2,
        )
        .await
        .unwrap();
    assert_eq!(first.len() + more.len(), 3);

    // Push rules, a pusher, receipts and filters.
    let rules = source.push_rules(None, 10).await.unwrap();
    assert_eq!(rules.len(), 1);
    let rules = &rules[0];
    assert_eq!(rules.custom.len(), 3, "{rules:?}");
    assert!(rules.unreadable.is_empty(), "{rules:?}");
    assert_eq!(rules.default_actions.len(), 1);
    assert!(
        rules
            .enabled
            .contains(&("override".into(), ".m.rule.suppress_notices".into(), false))
    );
    let pushers = source.pushers(None, 10).await.unwrap();
    assert_eq!(pushers.len(), 1);
    assert_eq!(pushers[0].pushkey, "alice-pushkey");
    let receipts = source.receipts(None, 10).await.unwrap();
    let first_message = facts["first_message"].as_str().unwrap();
    assert_eq!(
        receipts
            .iter()
            .map(|r| (
                r.receipt_type.as_str(),
                r.event_id.as_str(),
                r.thread_id.as_deref()
            ))
            .collect::<Vec<_>>(),
        [
            ("m.read", first_message, None),
            (
                "m.read.private",
                facts["private_receipt"].as_str().unwrap(),
                None
            ),
            (
                "m.read",
                facts["thread_receipt"].as_str().unwrap(),
                Some(first_message)
            ),
            (
                "m.read",
                facts["main_receipt"].as_str().unwrap(),
                Some("main")
            ),
        ]
    );
    let filters = source.filters(None, 10, "fixture.test").await.unwrap();
    assert_eq!(filters.len(), 2);
    assert_eq!(filters[0].0.user_id, "@alice:fixture.test");
    assert_eq!(filters[0].0.filter_id, "0");
    assert_eq!(filters[0].0.filter["room"]["timeline"]["limit"], 20);

    // Other servers' media Synapse had cached: one with its file under remote_content/<server>,
    // one whose file is gone (Synapse's cache eviction leaves the row, as the fixture does).
    let remote = source.remote_media(None, 10).await.unwrap();
    assert_eq!(
        remote
            .iter()
            .map(|m| format!("mxc://{}/{}", m.origin, m.media_id))
            .collect::<Vec<_>>(),
        [
            facts["remote_picture"].as_str().unwrap(),
            facts["remote_missing"].as_str().unwrap()
        ]
    );
    let picture = source
        .remote_media_bytes(&remote[0].origin, &remote[0].media_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(Some(picture.len() as u64), remote[0].length);
    assert!(
        source
            .remote_media_file_exists(&remote[0].origin, &remote[0].media_id)
            .await
    );
    assert_eq!(
        source
            .remote_media_bytes(&remote[1].origin, &remote[1].media_id)
            .await
            .unwrap(),
        None
    );
    assert!(
        !source
            .remote_media_file_exists(&remote[1].origin, &remote[1].media_id)
            .await
    );
    // The copy carries on from the checkpoint it last wrote.
    let key = remote[0].key();
    let after = SynapseRemoteMedia::parse_key(&key).unwrap();
    let rest = source.remote_media(Some(after), 10).await.unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].media_id, remote[1].media_id);
}

#[tokio::test]
async fn a_copy_is_verified_and_cut_over_and_nothing_can_follow_it() {
    let Some(fixture) = Fixture::load().await else {
        return;
    };
    let facts = facts();
    let rig = rig(Some(fixture.config.clone()), MemoryTarget::default(), None);
    let change = rig
        .migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap();
    assert_eq!(change.from, "idle");
    assert_eq!(change.status.status, "copying");
    let record = reaches(&rig.store, Phase::ReadyForCutover).await;
    assert_eq!(copied(&record, Stream::Users), 4);
    assert_eq!(copied(&record, Stream::Devices), 6);
    assert_eq!(copied(&record, Stream::AccessTokens), 6);
    assert_eq!(copied(&record, Stream::AccountData), 3);
    assert_eq!(copied(&record, Stream::E2eKeys), 2);
    assert_eq!(copied(&record, Stream::CrossSigning), 2);
    assert_eq!(copied(&record, Stream::KeyBackups), 2);
    assert_eq!(copied(&record, Stream::PushRules), 1);
    assert_eq!(copied(&record, Stream::Pushers), 1);
    assert_eq!(copied(&record, Stream::Filters), 2);
    assert_eq!(copied(&record, Stream::Rooms), 2);
    assert_eq!(copied(&record, Stream::Receipts), 4);
    assert_eq!(copied(&record, Stream::Media), 2);
    let remote = record.stream(Stream::RemoteMedia).unwrap();
    assert_eq!((remote.copied, remote.skipped), (1, 1), "{remote:?}");
    // The identity streams: the unspent refresh tokens (bob's exchanged one is left out), the
    // email and the phone number, the external identity, and two registration tokens; the two
    // to-device messages waiting for alice's phone, and not the one for a device nobody has.
    let refresh = record.stream(Stream::RefreshTokens).unwrap();
    assert_eq!((refresh.copied, refresh.skipped), (2, 1), "{refresh:?}");
    assert_eq!(copied(&record, Stream::Threepids), 2);
    assert_eq!(copied(&record, Stream::ExternalIds), 1);
    let to_device = record.stream(Stream::ToDevice).unwrap();
    assert_eq!(
        (to_device.copied, to_device.skipped),
        (2, 1),
        "{to_device:?}"
    );
    assert_eq!(copied(&record, Stream::RegistrationTokens), 2);
    assert!(record.streams.iter().all(|s| s.done && s.failed == 0));
    assert!(
        rig.target.users.lock().unwrap()["@dave:fixture.test"].erased,
        "dave was erased in Synapse"
    );
    assert!(
        !rig.target.users.lock().unwrap()["@alice:fixture.test"].erased,
        "alice was not"
    );
    assert_eq!(
        rig.target
            .refresh_tokens
            .lock()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            facts["alice_refresh_token"].as_str().unwrap(),
            facts["bob_refresh_token"].as_str().unwrap()
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(
        rig.target.threepids.lock().unwrap()
            [&("email".to_owned(), "alice@fixture.test".to_owned())],
        "@alice:fixture.test"
    );
    assert_eq!(
        rig.target.external_ids.lock().unwrap()[&(
            "oidc-fixture".to_owned(),
            "alice-at-the-provider".to_owned()
        )],
        "@alice:fixture.test"
    );
    assert_eq!(rig.target.to_device.lock().unwrap().len(), 2);
    assert_eq!(
        rig.target.registration_tokens.lock().unwrap()["fixture-token-one"].completed,
        2
    );
    assert_eq!(
        rig.target
            .remote_media
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        [("other.test".to_owned(), "RemoteCachedPictureOne".to_owned())]
    );

    // What landed: the keys, the backup's three room keys, the rules, the receipt.
    let target = &rig.target;
    let phone = target.device_keys.lock().unwrap()
        [&("@alice:fixture.test".to_owned(), "ALICEPHONE".to_owned())]
        .clone();
    assert_eq!(phone.one_time_keys.len(), 5);
    assert_eq!(
        target.backups.lock().unwrap()[&("@alice:fixture.test".to_owned(), 2)]
            .1
            .len(),
        3
    );
    assert_eq!(
        target.backups.lock().unwrap()[&("@alice:fixture.test".to_owned(), 1)]
            .1
            .len(),
        0,
        "a deleted version's keys are not copied"
    );
    let lobby = facts["lobby"].as_str().unwrap();
    assert_eq!(
        target.receipts.lock().unwrap()[&(
            lobby.to_owned(),
            "@bob:fixture.test".to_owned(),
            "m.read".to_owned(),
            None
        )]
            .0,
        facts["first_message"].as_str().unwrap()
    );
    // Alice's threaded receipts are copied in their threads, not as room receipts.
    for (thread, event) in [
        (facts["first_message"].as_str().unwrap(), "thread_receipt"),
        ("main", "main_receipt"),
    ] {
        assert_eq!(
            target.receipts.lock().unwrap()[&(
                lobby.to_owned(),
                "@alice:fixture.test".to_owned(),
                "m.read".to_owned(),
                Some(thread.to_owned())
            )]
                .0,
            facts[event].as_str().unwrap()
        );
    }
    // The rooms were written a page (`batch_size`, 2) at a time.
    let largest = target.largest_page.load(Ordering::Relaxed);
    assert!(largest > 0 && largest <= 2, "{largest}");

    // The log says what each stream carried and how fast the rooms went.
    let log = rig.store.log().await.unwrap();
    assert!(
        log.iter()
            .any(|e| e.stream == "e2e_keys" && e.message.contains("8 one-time keys")),
        "{log:#?}"
    );
    assert!(
        log.iter()
            .any(|e| e.stream == "key_backups" && e.message.contains("3 room keys stored")),
        "{log:#?}"
    );
    assert!(
        log.iter().any(|e| e.stream == "rooms"
            && e.message.starts_with("throughput: 2 rooms")
            && e.message.contains("events/s")
            && e.message.contains("peak memory")),
        "{log:#?}"
    );

    // Verification passes, and then notices a difference.
    rig.migrator.verify(&operator()).await.unwrap();
    let record = reaches(&rig.store, Phase::ReadyForCutover).await;
    let report = record.verification.clone().unwrap();
    assert!(report.passed, "{report:#?}");
    assert!(
        report
            .streams
            .iter()
            .any(|s| s.name == "events" && s.source_count > 30)
    );
    for (name, count) in [
        ("e2e_keys", 2),
        ("cross_signing", 2),
        ("key_backups", 2),
        ("push_rules", 1),
        ("pushers", 1),
        ("filters", 2),
        ("receipts", 4),
    ] {
        let stream = report.streams.iter().find(|s| s.name == name).unwrap();
        assert_eq!(
            (stream.source_count, stream.target_count),
            (count, count),
            "{stream:?}"
        );
    }
    rig.target
        .users
        .lock()
        .unwrap()
        .get_mut("@bob:fixture.test")
        .unwrap()
        .password_hash = Some("tampered".to_owned());
    rig.target
        .filters
        .lock()
        .unwrap()
        .insert(("@bob:fixture.test".to_owned(), "0".to_owned()), json!({}));
    rig.migrator.verify(&operator()).await.unwrap();
    // Verifying, then back to where it was.
    let record = loop {
        let record = reaches(&rig.store, Phase::ReadyForCutover).await;
        if record.verification.as_ref().is_some_and(|v| !v.passed) {
            break record;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let report = record.verification.unwrap();
    let users = &report.streams[0];
    assert!(
        users
            .mismatches
            .iter()
            .any(|m| m.contains("@bob:fixture.test"))
            || users.sampled < 4,
        "{users:?}"
    );
    let filters = report.streams.iter().find(|s| s.name == "filters").unwrap();
    assert_eq!(
        filters.mismatches,
        ["@bob:fixture.test's filter 0: differs"]
    );

    // The cutover's final pass brings bob's password and filter back, verifies, and completes.
    rig.migrator.cutover(&operator()).await.unwrap();
    let record = reaches(&rig.store, Phase::Completed).await;
    assert!(record.verification.as_ref().unwrap().passed);
    assert_eq!(record.cutover_by.as_deref(), Some("@ops:fixture.test"));
    assert_ne!(
        rig.target.users.lock().unwrap()["@bob:fixture.test"].password_hash,
        Some("tampered".to_owned())
    );
    for refused in [
        rig.migrator
            .start(&MigrationStartRequest::default(), &operator())
            .await
            .err(),
        rig.migrator.abort(&operator()).await.err(),
        rig.migrator.cutover(&operator()).await.err(),
    ] {
        assert!(
            matches!(refused, Some(SourceError::Conflict(_))),
            "{refused:?}"
        );
    }
    let log = rig.store.log().await.unwrap();
    assert!(
        log.iter()
            .any(|e| e.message.contains("cut over by @ops:fixture.test"))
    );
}

/// A room's events, served a page at a time from memory, in the order given.
struct Pages {
    events: Vec<SynapseEvent>,
    requests: AtomicUsize,
}

#[async_trait]
impl EventPages for Pages {
    async fn page(
        &self,
        after: Option<EventKey>,
        limit: i64,
    ) -> Result<Vec<(SynapseEvent, EventKey)>, MigrationError> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let start = after.map_or(0, |(_, i)| usize::try_from(i).unwrap() + 1);
        Ok(self
            .events
            .iter()
            .enumerate()
            .skip(start)
            .take(usize::try_from(limit).unwrap())
            .map(|(i, e)| (e.clone(), (0, i64::try_from(i).unwrap())))
            .collect())
    }
}

fn event(id: &str, kind: &str, prev: &[&str]) -> SynapseEvent {
    let json = json!({"type": kind, "state_key": "", "prev_events": prev, "auth_events": []});
    SynapseEvent {
        event_id: id.to_owned(),
        json_bytes: json.to_string().len() as u64,
        json,
        depth: 0,
        stream_ordering: 0,
        outlier: false,
        rejected: false,
    }
}

#[tokio::test]
async fn a_room_is_copied_a_page_at_a_time_and_an_event_before_its_parent_waits_for_it() {
    let room = SynapseRoom {
        room_id: "!r:fixture.test".into(),
        room_version: "11".into(),
        is_public: false,
        aliases: Vec::new(),
        redactions: Vec::new(),
    };
    // 1,000 events in a chain, except that `$late` (which `$early` cites) comes 500 events
    // after it, and `$orphan` cites an event the room does not have.
    let mut events = vec![event("$create", "m.room.create", &[])];
    for i in 1..1000 {
        let prev = format!("$e{}", i - 1);
        let prev = if i == 1 { "$create" } else { prev.as_str() };
        events.push(event(&format!("$e{i}"), "m.room.message", &[prev]));
    }
    events.insert(10, event("$early", "m.room.message", &["$late"]));
    events.insert(510, event("$late", "m.room.message", &["$e300"]));
    events.push(event("$orphan", "m.room.message", &["$nowhere"]));
    let pages = Pages {
        events,
        requests: AtomicUsize::new(0),
    };
    let target = MemoryTarget::default();
    let copy = copy_room(&pages, &target, &room, 50, &|| false)
        .await
        .unwrap();
    assert!(!copy.stopped);
    assert_eq!(copy.outcome.stored, 1002, "{:?}", copy.outcome.refused);
    assert_eq!(
        copy.outcome
            .refused
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["$orphan"]
    );
    assert!(copy.outcome.waiting.is_empty());
    // Never more than a page, or the few held aside, at once.
    let largest = target.largest_page.load(Ordering::Relaxed);
    assert!(largest <= 50, "{largest}");
    assert!(pages.requests.load(Ordering::Relaxed) >= 1003 / 50);
    assert_eq!(copy.stats.events_read, 1003);
    assert!(copy.stats.bytes > 1003 * 40);
    assert!(target.rooms.lock().unwrap()["!r:fixture.test"].finished);

    // A cancelled copy stops before its next page and leaves the room unfinished.
    let target = MemoryTarget::default();
    let calls = AtomicUsize::new(0);
    let cancel_after_two = || calls.fetch_add(1, Ordering::Relaxed) >= 2;
    let copy = copy_room(&pages, &target, &room, 50, &cancel_after_two)
        .await
        .unwrap();
    assert!(copy.stopped);
    assert_eq!(copy.stats.events_read, 100);
    assert!(!target.rooms.lock().unwrap()["!r:fixture.test"].finished);
}
#[tokio::test]
async fn a_paused_copy_resumes_where_it_stopped_and_an_abort_leaves_what_was_copied() {
    let Some(fixture) = Fixture::load().await else {
        return;
    };
    let gate = Arc::new(Semaphore::new(1));
    let target = MemoryTarget {
        gate: Some(gate.clone()),
        ..MemoryTarget::default()
    };
    let rig = rig(Some(fixture.config.clone()), target, None);
    rig.migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap();
    // One account copied; the copy waits on the second.
    let deadline = Instant::now() + Duration::from_secs(30);
    while rig.target.users.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "nothing was copied: {:#?}\n{:#?}",
            rig.store.load().await.unwrap(),
            rig.store.log().await.unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let task = rig.store.load().await.unwrap().task_id.unwrap();
    let paused = rig.migrator.pause(&operator()).await.unwrap().unwrap();
    assert_eq!(paused.status.status, "paused");
    assert!(rig.migrator.pause(&operator()).await.unwrap().is_none());
    assert_eq!(
        rig.tasks.get(&task).await.unwrap().unwrap().status,
        hs_admin::model::TaskStatus::Cancelled
    );
    // Nothing more is copied while paused, even with the gate open. The copy stops at its next
    // step, so the account it was waiting on may still land; nothing after it does.
    gate.add_permits(1_000);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let here: Vec<String> = rig.target.users.lock().unwrap().keys().cloned().collect();
    assert!(
        here.len() <= 2,
        "{here:?}\n{:#?}",
        rig.store.log().await.unwrap()
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.target.users.lock().unwrap().len(), here.len());
    assert!(rig.target.devices.lock().unwrap().is_empty());
    assert_eq!(copied(&rig.store.load().await.unwrap(), Stream::Users), 0);
    assert_eq!(rig.store.load().await.unwrap().phase, Phase::Paused);

    rig.migrator.resume(&operator()).await.unwrap().unwrap();
    let record = reaches(&rig.store, Phase::ReadyForCutover).await;
    assert_eq!(copied(&record, Stream::Users), 4, "{record:#?}");
    assert_eq!(copied(&record, Stream::Rooms), 2);

    // Abort, then start again: nothing is copied twice.
    let aborted = rig.migrator.abort(&operator()).await.unwrap().unwrap();
    assert_eq!(aborted.from, "ready_for_cutover");
    assert_eq!(aborted.status.status, "aborted");
    assert_eq!(rig.target.users.lock().unwrap().len(), 4);
    rig.migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap();
    let record = reaches(&rig.store, Phase::ReadyForCutover).await;
    assert_eq!(copied(&record, Stream::Users), 4);
}

#[tokio::test]
async fn a_copy_interrupted_by_a_restart_carries_on_in_the_next_process() {
    let Some(fixture) = Fixture::load().await else {
        return;
    };
    // The first process copied two accounts and stopped mid-copy.
    let store = Arc::new(InMemoryMigrationStore::new());
    let mut record = MigrationRecord {
        phase: Phase::Copying,
        source: Some(fixture.config.database.describe()),
        source_ref: Some("/migration/synapse".to_owned()),
        run: 7,
        task_id: Some("gone".to_owned()),
        ..MigrationRecord::default()
    };
    let users = record.stream_mut(Stream::Users);
    users.copied = 2;
    users.total = Some(4);
    users.checkpoint = Some("@bob:fixture.test".to_owned());
    store.save(&record).await.unwrap();

    let next = rig(
        Some(fixture.config.clone()),
        MemoryTarget::default(),
        Some(store),
    );
    assert_eq!(next.migrator.recover().await.unwrap(), Some(Phase::Copying));
    let record = reaches(&next.store, Phase::ReadyForCutover).await;
    // Only the accounts after the checkpoint were read again.
    let here: HashSet<String> = next.target.users.lock().unwrap().keys().cloned().collect();
    assert_eq!(
        here,
        HashSet::from([
            "@carol:fixture.test".to_owned(),
            "@dave:fixture.test".to_owned()
        ])
    );
    assert_eq!(copied(&record, Stream::Users), 4);
    assert!(record.run > 7);
    assert!(
        next.store
            .log()
            .await
            .unwrap()
            .iter()
            .any(|e| e.message.contains("restarted"))
    );
    // A second recover finds nothing running.
    assert_eq!(next.migrator.recover().await.unwrap(), None);
}

#[tokio::test]
async fn a_start_is_refused_without_a_source_or_for_another_server() {
    let none = rig(None, MemoryTarget::default(), None);
    let refused = none
        .migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, SourceError::Invalid(d) if d.contains("/migration/synapse")),
        "{refused:?}"
    );
    assert_eq!(none.store.load().await.unwrap().phase, Phase::Idle);

    let Some(fixture) = Fixture::load().await else {
        return;
    };
    let elsewhere = MemoryTarget {
        name: Some("other.example".to_owned()),
        ..MemoryTarget::default()
    };
    let store = Arc::new(InMemoryMigrationStore::new());
    let migrator = Migrator::new(MigratorParts {
        store: store.clone(),
        target: Arc::new(elsewhere),
        configs: Arc::new(Configs(Some(fixture.config.clone()))),
        tasks: TaskRegistry::in_memory(),
        events: None,
        observer: None,
        sample_size: 5,
    });
    let refused = migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, SourceError::Invalid(d) if d.contains("fixture.test") && d.contains("other.example")),
        "{refused:?}"
    );

    // And an unreachable database is said to be unreachable.
    let mut unreachable = fixture.config.clone();
    unreachable.database.port = 1;
    let rig = rig(Some(unreachable), MemoryTarget::default(), None);
    let refused = rig
        .migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, SourceError::Invalid(d) if d.contains("could not connect")),
        "{refused:?}"
    );
}

#[tokio::test]
async fn rooms_joined_over_federation_are_held_from_the_join_and_verified() {
    let Some(fixture) = Fixture::federated().await else {
        return;
    };
    let facts: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/synapse-federated/facts.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let elsewhere = facts["elsewhere"].as_str().unwrap();
    let faraway = facts["faraway"].as_str().unwrap();
    let source = SynapseSource::connect(&fixture.config).await.unwrap();

    // "Elsewhere" was read back to its beginning, so Synapse holds it whole: it is replayed from
    // its create event like a room made here. "Faraway" was not: Synapse holds it from hana's
    // join, with the state the remote server answered the join with.
    assert!(source.room_shape(elsewhere).await.unwrap().has_create);
    let shape = source.room_shape(faraway).await.unwrap();
    assert!(!shape.has_create, "{shape:?}");
    let join = source.remote_join(faraway).await.unwrap().unwrap();
    assert_eq!(join.join.json["state_key"], "@hana:127.0.0.1:18301");
    assert_eq!(join.join.json["content"]["membership"], "join");
    let state_types: HashSet<&str> = join
        .state
        .iter()
        .map(|e| e.json["type"].as_str().unwrap())
        .collect();
    for kind in [
        "m.room.create",
        "m.room.power_levels",
        "m.room.join_rules",
        "m.room.topic",
    ] {
        assert!(
            state_types.contains(kind),
            "{kind} missing from {state_types:?}"
        );
    }
    assert!(
        !join
            .state
            .iter()
            .any(|e| e.json["state_key"] == "@hana:127.0.0.1:18301"),
        "the state before the join holds no membership for hana"
    );
    // Everything the state and the join cite is in the state or its chain.
    let held: HashSet<&str> = join
        .state
        .iter()
        .chain(&join.auth_chain)
        .map(|e| e.event_id.as_str())
        .collect();
    for event in join.state.iter().chain(std::iter::once(&join.join)) {
        for id in event.json["auth_events"].as_array().unwrap() {
            assert!(held.contains(id.as_str().unwrap()), "{id} is not held");
        }
    }
    // Its history after the join: rita's welcome, hana's hello, the topic change, hugo's join
    // and two messages.
    assert_eq!(
        source
            .history_since(faraway, join.join_key.1)
            .await
            .unwrap(),
        6
    );

    let target = MemoryTarget {
        name: Some("127.0.0.1:18301".to_owned()),
        ..MemoryTarget::default()
    };
    let rig = rig(Some(fixture.config.clone()), target, None);
    rig.migrator
        .start(&MigrationStartRequest::default(), &operator())
        .await
        .unwrap();
    let record = reaches(&rig.store, Phase::ReadyForCutover).await;
    let rooms = record.stream(Stream::Rooms).unwrap();
    assert_eq!(
        (rooms.copied, rooms.skipped, rooms.failed),
        (2, 0, 0),
        "{record:#?}\n{:#?}",
        rig.store.log().await.unwrap()
    );
    assert_eq!(copied(&record, Stream::Receipts), 2);
    {
        let held = rig.target.rooms.lock().unwrap();
        let faraway_held = &held[faraway];
        assert!(faraway_held.event_ids.contains(&join.join.event_id));
        assert!(
            faraway_held
                .event_ids
                .contains(facts["faraway_last"].as_str().unwrap())
        );
        assert!(faraway_held.state.contains_key(&(
            "m.room.member".to_owned(),
            "@hugo:127.0.0.1:18301".to_owned()
        )));
    }
    let log = rig.store.log().await.unwrap();
    assert!(
        log.iter().any(|e| e.message.starts_with(&format!(
            "{faraway}: joined over federation by @hana:127.0.0.1:18301"
        ))),
        "{log:#?}"
    );

    // Verification compares each room's history from where it starts here, and its state.
    rig.migrator.verify(&operator()).await.unwrap();
    let record = loop {
        let record = reaches(&rig.store, Phase::ReadyForCutover).await;
        if record.verification.is_some() {
            break record;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let report = record.verification.unwrap();
    assert!(report.passed, "{report:#?}");
    let rooms = report.streams.iter().find(|s| s.name == "rooms").unwrap();
    assert_eq!((rooms.source_count, rooms.target_count), (2, 2));
}
