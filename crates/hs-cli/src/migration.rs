//! The migration from Synapse, wired into a running `hs serve`: the [`MigrationTarget`] over
//! this server's own stores, the durable [`MigrationStore`], the source read from the running
//! configuration, and the Prometheus families that follow a migration.
//!
//! The migration itself -- its state machine, the copy, verification and cutover -- is
//! `hs_compat::migration::Migrator`; the admin API's Migration area acts through it.
//!
//! # Where each stream lands
//!
//! - Accounts go straight into `hs-auth`'s user store with Synapse's bcrypt hash, flags,
//!   creation time and profile: `users.create` would re-hash, validate the localpart and stamp
//!   "now", none of which a migration wants.
//! - Devices and access tokens: `hs-auth`'s device and token stores. A token is kept by its
//!   SHA-256, as every token here is, so a client's `Authorization: Bearer syt_...` goes on
//!   working without a new sign-in.
//! - Account data: `hs-user`'s store (global and per room).
//! - Rooms: `hs-room`'s registry, each event through `RoomActorHandle::import_event` (authorized
//!   and stored as an event arriving over federation is, but announced to nobody), then the
//!   room is announced once, to `hs-user`'s session hub, so its members' `/sync` lists it.
//! - Media: `hs-media`'s object store and metadata, under the same media id.

use std::sync::Arc;

use async_trait::async_trait;
use hs_auth::store::{AccessTokenRecord, AuthStore, DeviceRecord, UserRecord};
use hs_auth::token::TokenHash;
use hs_compat::migration::model::{
    LogEntry, MigrationRecord, Phase, Stream, SynapseAccessToken, SynapseAccountData,
    SynapseDevice, SynapseEvent, SynapseMedia, SynapseRoom, SynapseUser,
};
use hs_compat::migration::{
    Imported, MigrationError, MigrationObserver, MigrationStore, MigrationTarget, RoomOutcome,
    SourceConfigs, TargetError, TargetMedia, TargetRoom, TargetUser,
};
use hs_config::migration::SynapseSourceConfig;
use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_media::id::MediaId;
use hs_media::metadata::MediaRecord;
use hs_media::repository::{MediaRepository, content_object_key};
use hs_room::RoomError;
use hs_room::actor::RemoteEventOutcome;
use hs_room::registry::RoomRegistry;
use hs_tables::keyspace::TypedKeyspace;
use hs_user::hub::SessionHub;
use object_store::ObjectStoreExt;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use serde_json::Value;

/// The session hub a running server has.
pub type Hub<B> = SessionHub<B, Arc<RoomRegistry<B>>>;

fn row(e: impl std::fmt::Display) -> TargetError {
    TargetError::row(e.to_string())
}

fn fatal(e: impl std::fmt::Display) -> TargetError {
    TargetError::fatal(e.to_string())
}

/// [`MigrationTarget`] over a running server's stores.
pub struct StoreTarget<B: KvBackend> {
    server_name: String,
    auth: Arc<dyn AuthStore>,
    hub: Arc<Hub<B>>,
    rooms: Arc<RoomRegistry<B>>,
    media: Arc<MediaRepository<B>>,
}

impl<B: KvBackend + 'static> StoreTarget<B> {
    /// A target over these stores, for a server named `server_name`.
    #[must_use]
    pub fn new(
        server_name: impl Into<String>,
        auth: Arc<dyn AuthStore>,
        hub: Arc<Hub<B>>,
        rooms: Arc<RoomRegistry<B>>,
        media: Arc<MediaRepository<B>>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            auth,
            hub,
            rooms,
            media,
        }
    }
}

fn user_id(raw: &str) -> Result<ruma::OwnedUserId, TargetError> {
    ruma::OwnedUserId::try_from(raw).map_err(|e| row(format!("{raw:?} is not a user id: {e}")))
}

#[async_trait]
impl<B: KvBackend + 'static> MigrationTarget for StoreTarget<B> {
    fn server_name(&self) -> &str {
        &self.server_name
    }

    async fn import_user(&self, user: &SynapseUser) -> Result<Imported, TargetError> {
        let id = user_id(&user.user_id)?;
        if id.server_name().as_str() != self.server_name {
            return Ok(Imported::Skipped(format!(
                "an account of another server ({})",
                id.server_name()
            )));
        }
        let existing = self.auth.get_user(&id).await.map_err(fatal)?;
        let Some(existing) = existing else {
            let mut record = UserRecord::new(id.clone(), user.created_at_ms);
            record.password_hash.clone_from(&user.password_hash);
            record.is_admin = user.admin;
            record.is_guest = user.guest;
            record.deactivated = user.deactivated;
            record.locked = user.locked;
            record.shadow_banned = user.shadow_banned;
            record.display_name.clone_from(&user.displayname);
            record.avatar_url.clone_from(&user.avatar_url);
            record.appservice_id.clone_from(&user.appservice_id);
            return match self.auth.create_user(record).await {
                Ok(()) => Ok(Imported::Created),
                Err(hs_auth::store::StoreError::Conflict(_)) => Err(row(
                    "another account here differs from it only by upper or lower case",
                )),
                Err(e) => Err(fatal(e)),
            };
        };
        let mut changed = false;
        if existing.password_hash != user.password_hash {
            self.auth
                .set_password_hash(&id, user.password_hash.clone())
                .await
                .map_err(fatal)?;
            changed = true;
        }
        if existing.is_admin != user.admin {
            self.auth.set_admin(&id, user.admin).await.map_err(fatal)?;
            changed = true;
        }
        if existing.deactivated != user.deactivated {
            self.auth
                .set_deactivated(&id, user.deactivated)
                .await
                .map_err(fatal)?;
            changed = true;
        }
        if existing.locked != user.locked {
            self.auth
                .set_locked(&id, user.locked)
                .await
                .map_err(fatal)?;
            changed = true;
        }
        if existing.display_name != user.displayname {
            self.auth
                .set_profile_display_name(&id, user.displayname.clone())
                .await
                .map_err(fatal)?;
            changed = true;
        }
        if existing.avatar_url != user.avatar_url {
            self.auth
                .set_profile_avatar_url(&id, user.avatar_url.clone())
                .await
                .map_err(fatal)?;
            changed = true;
        }
        Ok(if changed {
            Imported::Updated
        } else {
            Imported::AlreadyThere
        })
    }

    async fn import_device(&self, device: &SynapseDevice) -> Result<Imported, TargetError> {
        let id = user_id(&device.user_id)?;
        let device_id: ruma::OwnedDeviceId = device.device_id.as_str().into();
        if self.auth.get_user(&id).await.map_err(fatal)?.is_none() {
            return Ok(Imported::Skipped("its account was not copied".to_owned()));
        }
        let record = DeviceRecord {
            user_id: id.clone(),
            device_id: device_id.clone(),
            display_name: device.display_name.clone(),
            last_seen_ms: device.last_seen_ms,
            last_seen_ip: device.last_seen_ip.clone(),
        };
        let existing = self.auth.get_device(&id, &device_id).await.map_err(fatal)?;
        if existing
            .as_ref()
            .is_some_and(|d| d.display_name == record.display_name)
        {
            return Ok(Imported::AlreadyThere);
        }
        self.auth.upsert_device(record).await.map_err(fatal)?;
        Ok(if existing.is_some() {
            Imported::Updated
        } else {
            Imported::Created
        })
    }

    async fn import_access_token(
        &self,
        token: &SynapseAccessToken,
    ) -> Result<Imported, TargetError> {
        let id = user_id(&token.user_id)?;
        let hash = TokenHash::of(&token.token);
        if let Some(existing) = self.auth.get_access_token(&hash).await.map_err(fatal)? {
            return if existing.user_id == id {
                Ok(Imported::AlreadyThere)
            } else {
                Err(row("the same token already signs in another account here"))
            };
        }
        if self.auth.get_user(&id).await.map_err(fatal)?.is_none() {
            return Ok(Imported::Skipped("its account was not copied".to_owned()));
        }
        self.auth
            .put_access_token(AccessTokenRecord {
                hash,
                user_id: id,
                device_id: token.device_id.as_deref().map(Into::into),
                expires_at_ms: token.valid_until_ms,
                refresh_token_hash: None,
                last_used_ms: None,
            })
            .await
            .map_err(fatal)?;
        Ok(Imported::Created)
    }

    async fn import_account_data(
        &self,
        data: &SynapseAccountData,
    ) -> Result<Imported, TargetError> {
        let id = user_id(&data.user_id)?;
        if self.auth.get_user(&id).await.map_err(fatal)?.is_none() {
            return Ok(Imported::Skipped("its account was not copied".to_owned()));
        }
        let store = self.hub.store();
        match &data.room_id {
            None => {
                let existing = store
                    .get_global_account_data(&id, &data.data_type)
                    .await
                    .map_err(fatal)?;
                if existing.as_ref().is_some_and(|r| r.content == data.content) {
                    return Ok(Imported::AlreadyThere);
                }
                store
                    .put_global_account_data(&id, &data.data_type, data.content.clone())
                    .await
                    .map_err(fatal)?;
                Ok(if existing.is_some() {
                    Imported::Updated
                } else {
                    Imported::Created
                })
            }
            Some(room) => {
                let room_id = ruma::OwnedRoomId::try_from(room.as_str())
                    .map_err(|e| row(format!("{room:?} is not a room id: {e}")))?;
                let existing = store
                    .list_room_account_data(&id, &room_id)
                    .await
                    .map_err(fatal)?
                    .into_iter()
                    .find(|r| r.event_type == data.data_type);
                if existing.as_ref().is_some_and(|r| r.content == data.content) {
                    return Ok(Imported::AlreadyThere);
                }
                store
                    .put_room_account_data(&id, &room_id, &data.data_type, data.content.clone())
                    .await
                    .map_err(fatal)?;
                Ok(if existing.is_some() {
                    Imported::Updated
                } else {
                    Imported::Created
                })
            }
        }
    }

    async fn import_room(
        &self,
        room: &SynapseRoom,
        events: &[&SynapseEvent],
    ) -> Result<RoomOutcome, TargetError> {
        let room_id = ruma::OwnedRoomId::try_from(room.room_id.as_str())
            .map_err(|e| row(format!("{:?} is not a room id: {e}", room.room_id)))?;
        let version = ruma::RoomVersionId::try_from(room.room_version.as_str())
            .map_err(|e| row(format!("room version {:?}: {e}", room.room_version)))?;
        let handle = match self.rooms.import_shell(&room_id, version.clone()).await {
            Ok(handle) => handle,
            Err(RoomError::UnsupportedRoomVersion(v)) => {
                return Err(row(format!("room version {v} is not supported here")));
            }
            Err(e) => return Err(fatal(e)),
        };
        let mut outcome = RoomOutcome::default();
        for event in events {
            let parsed = match hs_model::Event::parse(&event.json, version.clone()) {
                Ok(parsed) => parsed,
                Err(e) => {
                    outcome
                        .refused
                        .push((event.event_id.clone(), format!("unreadable: {e}")));
                    continue;
                }
            };
            if parsed.event_id().as_str() != event.event_id {
                outcome.refused.push((
                    event.event_id.clone(),
                    format!("its content hashes to {}, not to its id", parsed.event_id()),
                ));
                continue;
            }
            match handle.import_event(parsed).await {
                Ok(RemoteEventOutcome::Stored(_)) => outcome.stored += 1,
                Ok(RemoteEventOutcome::AlreadyKnown) => outcome.already_there += 1,
                Err(RoomError::Store(e)) => return Err(fatal(e)),
                Err(e) => outcome
                    .refused
                    .push((event.event_id.clone(), e.to_string())),
            }
        }
        let held = handle.query(|a| a.head_update().is_some()).await;
        if !held {
            self.rooms.discard_import_shell(&room_id, &handle).await;
            let why = outcome.refused.first().map_or_else(
                || "it has no events".to_owned(),
                |(id, why)| format!("{id}: {why}"),
            );
            return Err(row(format!("its first event was refused ({why})")));
        }
        for (redaction, target) in &room.redactions {
            let (Ok(redaction), Ok(target)) = (
                ruma::OwnedEventId::try_from(redaction.as_str()),
                ruma::OwnedEventId::try_from(target.as_str()),
            ) else {
                continue;
            };
            let both = handle
                .query({
                    let redaction = redaction.clone();
                    let target = target.clone();
                    move |a| {
                        let redacted = a
                            .event_by_id(&target)
                            .map(|e| e.header().flags.is_redacted());
                        (a.event_by_id(&redaction).is_some(), redacted)
                    }
                })
                .await;
            // Only a redaction this server holds, of an event it holds and has not yet redacted.
            if both == (true, Some(false)) {
                handle.import_redaction(target).await.map_err(row)?;
                outcome.redactions += 1;
            }
        }
        for (alias, creator) in &room.aliases {
            let Ok(alias) = ruma::OwnedRoomAliasId::try_from(alias.as_str()) else {
                continue;
            };
            if self.rooms.resolve_alias(&alias).map_err(fatal)?.is_some() {
                continue;
            }
            let creator = creator
                .as_deref()
                .and_then(|c| ruma::OwnedUserId::try_from(c).ok())
                .or_else(|| {
                    ruma::OwnedUserId::try_from(format!("@_migration:{}", self.server_name)).ok()
                });
            let Some(creator) = creator else { continue };
            let made = handle
                .query(move |a| a.create_alias(&alias, &creator))
                .await;
            match made {
                Ok(()) => outcome.aliases += 1,
                Err(RoomError::RoomAlreadyExists(_)) => {}
                Err(e) => return Err(row(e)),
            }
        }
        if room.is_public {
            self.rooms
                .set_directory_visibility(&room_id, true)
                .map_err(row)?;
        }
        // Announced once, now that all of it is in: the hub lists the room in the `/sync` of
        // everyone its current state says is a member.
        if let Some(head) = handle.query(|a| a.head_update()).await {
            self.hub.process_room_update(head).await.map_err(row)?;
        }
        Ok(outcome)
    }

    async fn import_media(
        &self,
        media: &SynapseMedia,
        bytes: Option<Vec<u8>>,
    ) -> Result<Imported, TargetError> {
        let id = MediaId::parse(&media.media_id)
            .map_err(|e| row(format!("{:?} is not a media id here: {e}", media.media_id)))?;
        let server = self.media.server_name().to_owned();
        let existing = self
            .media
            .metadata()
            .get_media(&server, &media.media_id)
            .map_err(fatal)?;
        if existing.as_ref().is_some_and(|m| m.completed) {
            return Ok(Imported::AlreadyThere);
        }
        let Some(bytes) = bytes else {
            return Ok(Imported::Skipped(
                "its file is not in the media store (mount Synapse's media_store_path and start \
                 again to copy it)"
                    .to_owned(),
            ));
        };
        let length = bytes.len() as u64;
        self.media
            .object_store()
            .put(&content_object_key(&server, &id), bytes.into())
            .await
            .map_err(fatal)?;
        self.media
            .metadata()
            .put_media(&MediaRecord {
                server_name: server,
                media_id: media.media_id.clone(),
                content_type: media
                    .content_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                upload_name: media.upload_name.clone(),
                byte_length: Some(length),
                created_ms: media.created_ms,
                uploader: media.uploader.clone(),
                completed: true,
                expires_at_ms: None,
                quarantined_by: media.quarantined_by.clone(),
                safe_from_quarantine: media.safe_from_quarantine,
                last_accessed_ms: None,
            })
            .map_err(fatal)?;
        Ok(Imported::Created)
    }

    async fn user(&self, user_id_raw: &str) -> Result<Option<TargetUser>, TargetError> {
        let Ok(id) = ruma::OwnedUserId::try_from(user_id_raw) else {
            return Ok(None);
        };
        Ok(self
            .auth
            .get_user(&id)
            .await
            .map_err(fatal)?
            .map(|u| TargetUser {
                password_hash: u.password_hash,
                displayname: u.display_name,
                avatar_url: u.avatar_url,
                admin: u.is_admin,
                deactivated: u.deactivated,
            }))
    }

    async fn device(
        &self,
        user_id_raw: &str,
        device_id: &str,
    ) -> Result<Option<Option<String>>, TargetError> {
        let Ok(id) = ruma::OwnedUserId::try_from(user_id_raw) else {
            return Ok(None);
        };
        let device_id: ruma::OwnedDeviceId = device_id.into();
        Ok(self
            .auth
            .get_device(&id, &device_id)
            .await
            .map_err(fatal)?
            .map(|d| d.display_name))
    }

    async fn access_token(
        &self,
        token: &str,
    ) -> Result<Option<(String, Option<String>)>, TargetError> {
        Ok(self
            .auth
            .get_access_token(&TokenHash::of(token))
            .await
            .map_err(fatal)?
            .map(|t| (t.user_id.to_string(), t.device_id.map(|d| d.to_string()))))
    }

    async fn account_data(
        &self,
        user_id_raw: &str,
        room_id: Option<&str>,
        data_type: &str,
    ) -> Result<Option<Value>, TargetError> {
        let Ok(id) = ruma::OwnedUserId::try_from(user_id_raw) else {
            return Ok(None);
        };
        let store = self.hub.store();
        match room_id {
            None => Ok(store
                .get_global_account_data(&id, data_type)
                .await
                .map_err(fatal)?
                .map(|r| r.content)),
            Some(room) => {
                let Ok(room_id) = ruma::OwnedRoomId::try_from(room) else {
                    return Ok(None);
                };
                Ok(store
                    .list_room_account_data(&id, &room_id)
                    .await
                    .map_err(fatal)?
                    .into_iter()
                    .find(|r| r.event_type == data_type)
                    .map(|r| r.content))
            }
        }
    }

    async fn room(&self, room_id_raw: &str) -> Result<Option<TargetRoom>, TargetError> {
        let Ok(room_id) = ruma::OwnedRoomId::try_from(room_id_raw) else {
            return Ok(None);
        };
        let handle = match self.rooms.get_or_load(&room_id).await {
            Ok(handle) => handle,
            Err(RoomError::RoomNotFound(_)) => return Ok(None),
            Err(e) => return Err(fatal(e)),
        };
        let room = handle
            .query(|a| -> Result<TargetRoom, RoomError> {
                let event_ids = a
                    .events_after(0, usize::MAX)
                    .into_iter()
                    .map(|(_, e)| e.event_id().to_string())
                    .collect();
                let current_state = a
                    .full_state()?
                    .into_iter()
                    .map(|e| {
                        (
                            (
                                e.header().event_type.clone(),
                                e.header().state_key.clone().unwrap_or_default(),
                            ),
                            e.event_id().to_string(),
                        )
                    })
                    .collect();
                Ok(TargetRoom {
                    event_ids,
                    current_state,
                })
            })
            .await
            .map_err(fatal)?;
        Ok(Some(room))
    }

    async fn media(&self, media_id: &str) -> Result<Option<TargetMedia>, TargetError> {
        let server = self.media.server_name().to_owned();
        let Some(record) = self
            .media
            .metadata()
            .get_media(&server, media_id)
            .map_err(fatal)?
        else {
            return Ok(None);
        };
        let bytes = match MediaId::parse(media_id) {
            Ok(id) => match self
                .media
                .object_store()
                .get(&content_object_key(&server, &id))
                .await
            {
                Ok(found) => found.bytes().await.ok().map(|b| b.to_vec()),
                Err(_) => None,
            },
            Err(_) => None,
        };
        Ok(Some(TargetMedia {
            content_type: record.content_type,
            bytes,
        }))
    }
}

// -------------------------------------------------------------------------------------------
// The durable record and log.
// -------------------------------------------------------------------------------------------

/// [`MigrationStore`] over `hs-kv`: the record in `hs_compat.migration` (one row), the log in
/// `hs_compat.migration_log`, keyed by a sequence number.
pub struct TablesMigrationStore<B: KvBackend> {
    backend: B,
    record: TypedKeyspace<B::Keyspace, (String,)>,
    log: TypedKeyspace<B::Keyspace, (u64,)>,
    /// Serializes log appends in this process, so sequence numbers are not handed out twice.
    appending: tokio::sync::Mutex<()>,
}

impl<B: KvBackend> TablesMigrationStore<B> {
    /// Opens (or creates) the keyspaces.
    ///
    /// # Errors
    /// The backend's.
    pub fn open(backend: B) -> Result<Self, hs_kv::KvError> {
        Ok(Self {
            record: TypedKeyspace::new(backend.keyspace("hs_compat.migration")?),
            log: TypedKeyspace::new(backend.keyspace("hs_compat.migration_log")?),
            backend,
            appending: tokio::sync::Mutex::new(()),
        })
    }
}

fn store_err(e: impl std::fmt::Display) -> MigrationError {
    MigrationError::Store(e.to_string())
}

const RECORD_KEY: &str = "synapse";

#[async_trait]
impl<B: KvBackend + 'static> MigrationStore for TablesMigrationStore<B> {
    async fn load(&self) -> Result<MigrationRecord, MigrationError> {
        let snapshot = self.backend.snapshot();
        match self
            .record
            .get(&snapshot, &(RECORD_KEY.to_owned(),))
            .map_err(store_err)?
        {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(store_err),
            None => Ok(MigrationRecord::default()),
        }
    }

    async fn save(&self, record: &MigrationRecord) -> Result<(), MigrationError> {
        let value = serde_json::to_vec(record).map_err(store_err)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            self.record
                .put(txn, &(RECORD_KEY.to_owned(),), &value)
                .map_err(hs_kv::KvError::backend)
        })
        .map_err(store_err)
    }

    async fn append_log(&self, entries: &[LogEntry]) -> Result<(), MigrationError> {
        let _guard = self.appending.lock().await;
        let snapshot = self.backend.snapshot();
        let last = self
            .log
            .range(&snapshot, RangeSpec::full().reverse().limit(1))
            .next()
            .transpose()
            .map_err(store_err)?
            .map_or(0, |((seq,), _)| seq);
        let first_kept = (last + entries.len() as u64)
            .saturating_sub(hs_compat::migration::store::LOG_LIMIT as u64);
        let stale: Vec<(u64,)> = self
            .log
            .range(&snapshot, RangeSpec::full().limit(entries.len().max(1)))
            .filter_map(|item| item.ok().map(|(key, _)| key))
            .filter(|(seq,)| *seq <= first_kept)
            .collect();
        let values: Vec<Vec<u8>> = entries
            .iter()
            .map(serde_json::to_vec)
            .collect::<Result<_, _>>()
            .map_err(store_err)?;
        transact(&self.backend, TransactConfig::default(), |txn| {
            for key in &stale {
                self.log.delete(txn, key).map_err(hs_kv::KvError::backend)?;
            }
            for (i, value) in values.iter().enumerate() {
                self.log
                    .put(txn, &(last + 1 + i as u64,), value)
                    .map_err(hs_kv::KvError::backend)?;
            }
            Ok(())
        })
        .map_err(store_err)
    }

    async fn log(&self) -> Result<Vec<LogEntry>, MigrationError> {
        let snapshot = self.backend.snapshot();
        self.log
            .range(&snapshot, RangeSpec::full())
            .map(|item| {
                let (_, value) = item.map_err(store_err)?;
                serde_json::from_slice(&value).map_err(store_err)
            })
            .collect()
    }
}

// -------------------------------------------------------------------------------------------
// The source, from the running configuration.
// -------------------------------------------------------------------------------------------

/// Reads the migration source out of the configuration store the admin API writes, so a source
/// an operator set a moment ago on the Migration page is the one a start uses.
pub struct StoreSourceConfigs(pub Arc<crate::config_source::StoreConfigSource>);

/// Reads it out of the configuration this process booted on (a server without a configuration
/// store: the in-process tests).
pub struct BootedSourceConfigs(pub hs_config::Config);

fn pick(config: &hs_config::Config, pointer: &str) -> Result<Option<SynapseSourceConfig>, String> {
    let document = serde_json::to_value(config).map_err(|e| e.to_string())?;
    match document.pointer(pointer) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|e| e.to_string()),
    }
}

#[async_trait]
impl SourceConfigs for StoreSourceConfigs {
    async fn source(&self, pointer: &str) -> Result<Option<SynapseSourceConfig>, String> {
        let config = self.0.current_config().await.map_err(|e| e.to_string())?;
        pick(&config, pointer)
    }
}

#[async_trait]
impl SourceConfigs for BootedSourceConfigs {
    async fn source(&self, pointer: &str) -> Result<Option<SynapseSourceConfig>, String> {
        pick(&self.0, pointer)
    }
}

// -------------------------------------------------------------------------------------------
// Metrics.
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct StreamLabels {
    stream: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct StatusLabels {
    status: String,
}

/// The migration's Prometheus families: `hs_migration_rows_copied`, `_rows_skipped`,
/// `_rows_failed` and `_rows_source` per stream, and `hs_migration_status` (1 for the current
/// status, 0 for the others).
#[derive(Clone)]
pub struct MigrationMetrics {
    copied: Family<StreamLabels, Gauge>,
    skipped: Family<StreamLabels, Gauge>,
    failed: Family<StreamLabels, Gauge>,
    source: Family<StreamLabels, Gauge>,
    status: Family<StatusLabels, Gauge>,
}

impl MigrationMetrics {
    /// Registers the families into `metrics`'s shared registry.
    #[must_use]
    pub fn register(metrics: &hs_telemetry::metrics::Metrics) -> Self {
        let this = Self {
            copied: Family::default(),
            skipped: Family::default(),
            failed: Family::default(),
            source: Family::default(),
            status: Family::default(),
        };
        metrics.with_registry(|registry| {
            registry.register(
                "hs_migration_rows_copied",
                "Rows of each stream of the migration from Synapse that are here",
                this.copied.clone(),
            );
            registry.register(
                "hs_migration_rows_skipped",
                "Rows of each stream deliberately not copied (see the migration log)",
                this.skipped.clone(),
            );
            registry.register(
                "hs_migration_rows_failed",
                "Rows of each stream that could not be copied (see the migration log)",
                this.failed.clone(),
            );
            registry.register(
                "hs_migration_rows_source",
                "Rows of each stream in Synapse, as counted when the stream started",
                this.source.clone(),
            );
            registry.register(
                "hs_migration_status",
                "1 for the migration's current status, 0 for every other",
                this.status.clone(),
            );
        });
        this
    }
}

impl MigrationObserver for MigrationMetrics {
    fn observe(&self, record: &MigrationRecord) {
        for phase in Phase::ALL {
            self.status
                .get_or_create(&StatusLabels {
                    status: phase.as_str().to_owned(),
                })
                .set(i64::from(phase == record.phase));
        }
        for stream in Stream::ALL {
            let labels = StreamLabels {
                stream: stream.as_str().to_owned(),
            };
            let progress = record.stream(stream);
            let as_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
            self.copied
                .get_or_create(&labels)
                .set(as_i64(progress.map_or(0, |s| s.copied)));
            self.skipped
                .get_or_create(&labels)
                .set(as_i64(progress.map_or(0, |s| s.skipped)));
            self.failed
                .get_or_create(&labels)
                .set(as_i64(progress.map_or(0, |s| s.failed)));
            self.source
                .get_or_create(&labels)
                .set(as_i64(progress.and_then(|s| s.total).unwrap_or(0)));
        }
    }
}

// -------------------------------------------------------------------------------------------
// Putting it together.
// -------------------------------------------------------------------------------------------

/// What [`build`] needs from a booting server.
pub struct MigrationParts<'a, B: KvBackend> {
    /// The server's store: the migration's record and log are kept in it.
    pub backend: B,
    /// This server's name.
    pub server_name: String,
    /// Accounts, devices and tokens.
    pub auth: Arc<dyn AuthStore>,
    /// Account data, and who is in which room for `/sync`.
    pub hub: Arc<Hub<B>>,
    /// Rooms.
    pub rooms: Arc<RoomRegistry<B>>,
    /// Media.
    pub media: Arc<MediaRepository<B>>,
    /// Where the migration's steps run.
    pub tasks: Arc<hs_admin::tasks::TaskRegistry>,
    /// The admin API's event stream.
    pub events: Arc<hs_admin::events::EventBus>,
    /// Where the Prometheus families are registered.
    pub metrics: &'a hs_telemetry::metrics::Metrics,
    /// Where the source is read from.
    pub configs: Arc<dyn SourceConfigs>,
}

/// The migration from Synapse for a running server.
///
/// # Errors
/// The backend's, if the migration's keyspaces cannot be opened.
pub fn build<B: KvBackend + 'static>(
    parts: MigrationParts<'_, B>,
) -> Result<Arc<hs_compat::migration::Migrator>, hs_kv::KvError> {
    let store = Arc::new(TablesMigrationStore::open(parts.backend)?);
    let target = Arc::new(StoreTarget::new(
        parts.server_name,
        parts.auth,
        parts.hub,
        parts.rooms,
        parts.media,
    ));
    Ok(hs_compat::migration::Migrator::new(
        hs_compat::migration::engine::MigratorParts {
            store,
            target,
            configs: parts.configs,
            tasks: parts.tasks,
            events: Some(parts.events),
            observer: Some(Arc::new(MigrationMetrics::register(parts.metrics))),
            sample_size: 25,
        },
    ))
}
