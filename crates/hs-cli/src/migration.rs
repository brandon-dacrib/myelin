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
//! - End-to-end keys, cross-signing keys and key backups: `hs-e2e`'s store, through its own
//!   operations (an upload, a claim, a backup version created), so the device-list stream and a
//!   backup's counts move as they would for a client. A device's one-time keys are made the
//!   same as Synapse's: new ones uploaded, and as many as Synapse handed out since the last pass
//!   claimed here, oldest first (the order both servers hand them out in).
//! - Push rules and pushers: `hs-push`'s ruleset and pusher stores; a ruleset is the server
//!   default with the account's own rules, changed actions and on/off flags applied to it.
//! - Filters: `hs-user`'s store, under Synapse's ids (`UserStore::import_filter`).
//! - Rooms: `hs-room`'s registry, a page of events at a time, each through
//!   `RoomActorHandle::import_event` (authorized and stored as an event arriving over federation
//!   is, but announced to nobody), then the room is announced once, to `hs-user`'s session hub,
//!   so its members' `/sync` lists it.
//! - Receipts: `hs-user`'s receipt store through `SessionHub::import_receipt` (in `/sync`, and
//!   on the receipt stream appservices read), not sent to other servers again.
//! - Media: `hs-media`'s object store and metadata, under the same media id.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use hs_auth::store::{AccessTokenRecord, AuthStore, DeviceRecord, UserRecord};
use hs_auth::token::TokenHash;
use hs_compat::migration::model::{
    LogEntry, MigrationRecord, Phase, Stream, SynapseAccessToken, SynapseAccountData,
    SynapseBackupVersion, SynapseCrossSigning, SynapseDevice, SynapseDeviceKeys, SynapseEvent,
    SynapseFilter, SynapseMedia, SynapsePushRules, SynapsePusher, SynapseReceipt,
    SynapseRemoteJoin, SynapseRoom, SynapseRoomKey, SynapseUser,
};
use hs_compat::migration::{
    Check, CurrentState, Imported, MigrationError, MigrationObserver, MigrationStore,
    MigrationTarget, RoomOutcome, RoomStats, SourceConfigs, TargetError, TargetMedia, TargetUser,
};
use hs_config::migration::SynapseSourceConfig;
use hs_e2e::store::{BackupSessionRow, CrossSigningKeyType, E2eStore};
use hs_kv::{KvBackend, RangeSpec, TransactConfig, transact};
use hs_media::id::MediaId;
use hs_media::metadata::MediaRecord;
use hs_media::repository::{MediaRepository, content_object_key};
use hs_push::pushers::PusherStore;
use hs_push::ruleset::{NewRule, RuleKind, Ruleset};
use hs_push::rulesets::tables::TablesRulesetStore;
use hs_push::rulesets::{CachedRulesetStore, RulesetStore};
use hs_room::RoomError;
use hs_room::actor::RemoteEventOutcome;
use hs_room::registry::RoomRegistry;
use hs_tables::keyspace::TypedKeyspace;
use hs_user::hub::SessionHub;
use hs_user::receipts::ReceiptKind;
use object_store::ObjectStoreExt;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use ruma::push::{Action, PushCondition};
use serde_json::Value;

/// The session hub a running server has.
pub type Hub<B> = SessionHub<B, Arc<RoomRegistry<B>>>;

/// A running server's push rules, cached as `hs-push` reads them.
pub type Rulesets<B> = Arc<CachedRulesetStore<TablesRulesetStore<B>>>;

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
    e2e: Arc<dyn E2eStore>,
    rulesets: Rulesets<B>,
    pushers: Arc<dyn PusherStore>,
}

/// The stores a [`StoreTarget`] writes, beyond accounts, rooms and media.
pub struct SessionStores<B: KvBackend> {
    /// End-to-end keys, cross-signing keys and key backups.
    pub e2e: Arc<dyn E2eStore>,
    /// Push rules.
    pub rulesets: Rulesets<B>,
    /// Pushers.
    pub pushers: Arc<dyn PusherStore>,
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
        sessions: SessionStores<B>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            auth,
            hub,
            rooms,
            media,
            e2e: sessions.e2e,
            rulesets: sessions.rulesets,
            pushers: sessions.pushers,
        }
    }

    /// `raw` as a user id of this server whose account was copied: `Err(Skipped)` with why not.
    async fn local_account(
        &self,
        raw: &str,
    ) -> Result<Result<ruma::OwnedUserId, Imported>, TargetError> {
        let id = user_id(raw)?;
        if id.server_name().as_str() != self.server_name {
            return Ok(Err(Imported::Skipped(format!(
                "an account of another server ({})",
                id.server_name()
            ))));
        }
        if self.auth.get_user(&id).await.map_err(fatal)?.is_none() {
            return Ok(Err(Imported::Skipped(
                "its account was not copied".to_owned(),
            )));
        }
        Ok(Ok(id))
    }

    /// The room `room` is imported into: the one here, or a new shell for it.
    async fn import_handle(
        &self,
        room: &SynapseRoom,
    ) -> Result<(hs_room::actor::RoomActorHandle<B>, ruma::RoomVersionId), TargetError> {
        let room_id = ruma::OwnedRoomId::try_from(room.room_id.as_str())
            .map_err(|e| row(format!("{:?} is not a room id: {e}", room.room_id)))?;
        let version = ruma::RoomVersionId::try_from(room.room_version.as_str())
            .map_err(|e| row(format!("room version {:?}: {e}", room.room_version)))?;
        match self.rooms.import_shell(&room_id, version.clone()).await {
            Ok(handle) => Ok((handle, version)),
            Err(RoomError::UnsupportedRoomVersion(v)) => {
                Err(row(format!("room version {v} is not supported here")))
            }
            Err(e) => Err(fatal(e)),
        }
    }

    /// The push rules `rules` describe, applied to the server default.
    fn ruleset_for(user: &ruma::UserId, rules: &SynapsePushRules) -> (Ruleset, Vec<String>) {
        let mut ruleset = hs_push::rulesets::default_ruleset(user);
        let mut notes = Vec::new();
        // Lowest priority first: each newly inserted rule becomes its kind's highest.
        for (kind, rule) in rules.custom.iter().rev() {
            match new_push_rule(kind, rule) {
                Ok(new) => {
                    if let Err(e) = ruleset.insert(new, None, None) {
                        notes.push(format!("{kind} rule {}: {e}", rule["rule_id"]));
                    }
                }
                Err(why) => notes.push(format!("{kind} rule {}: {why}", rule["rule_id"])),
            }
        }
        for (kind, id, actions) in &rules.default_actions {
            let actions: Vec<Action> = match serde_json::from_value(actions.clone()) {
                Ok(actions) => actions,
                Err(e) => {
                    notes.push(format!("the actions of {kind} rule {id}: {e}"));
                    continue;
                }
            };
            if RuleKind::parse(kind)
                .and_then(|k| ruleset.set_actions(k, id, actions).ok())
                .is_none()
            {
                notes.push(format!(
                    "{kind} rule {id}: Synapse has this server-default rule, and this server does \
                     not (it was retired from the specification)"
                ));
            }
        }
        for (kind, id, enabled) in &rules.enabled {
            if RuleKind::parse(kind)
                .and_then(|k| ruleset.set_enabled(k, id, *enabled).ok())
                .is_none()
            {
                notes.push(format!(
                    "{kind} rule {id} was turned {} in Synapse, and there is no such rule here",
                    if *enabled { "on" } else { "off" }
                ));
            }
        }
        (ruleset, notes)
    }

    /// One-time-key counts per algorithm of a device's Synapse keys.
    fn synapse_otk_counts(keys: &SynapseDeviceKeys) -> BTreeMap<String, u64> {
        let mut counts = BTreeMap::new();
        for (id, _) in &keys.one_time_keys {
            let algorithm = id.split_once(':').map_or(id.as_str(), |(a, _)| a);
            *counts.entry(algorithm.to_owned()).or_insert(0) += 1;
        }
        counts
    }

    /// The fallback-key algorithms Synapse has an unused key for.
    fn synapse_unused_fallback(keys: &SynapseDeviceKeys) -> BTreeSet<String> {
        keys.fallback_keys
            .iter()
            .filter(|(_, _, used)| !used)
            .map(|(id, _, _)| {
                id.split_once(':')
                    .map_or(id.as_str(), |(a, _)| a)
                    .to_owned()
            })
            .collect()
    }
}

fn user_id(raw: &str) -> Result<ruma::OwnedUserId, TargetError> {
    ruma::OwnedUserId::try_from(raw).map_err(|e| row(format!("{raw:?} is not a user id: {e}")))
}

/// A rule as `PUT /pushrules/global/{kind}/{ruleId}` would make it.
fn new_push_rule(kind: &str, rule: &Value) -> Result<NewRule, String> {
    let id = rule["rule_id"].as_str().ok_or("no rule_id")?.to_owned();
    let actions: Vec<Action> =
        serde_json::from_value(rule["actions"].clone()).map_err(|e| format!("actions: {e}"))?;
    let kind = RuleKind::parse(kind).ok_or_else(|| format!("no {kind} rules here"))?;
    let conditions: Vec<PushCondition> = match kind {
        RuleKind::Override | RuleKind::Underride => {
            serde_json::from_value(rule["conditions"].clone())
                .map_err(|e| format!("conditions: {e}"))?
        }
        _ => Vec::new(),
    };
    let pattern = match kind {
        RuleKind::Content => Some(rule["pattern"].as_str().ok_or("no pattern")?.to_owned()),
        _ => None,
    };
    Ok(NewRule {
        kind,
        rule_id: id,
        actions,
        conditions,
        pattern,
    })
}

/// A pusher as `POST /pushers/set` would make it.
fn pusher_record(pusher: &SynapsePusher) -> Result<ruma::api::client::push::Pusher, String> {
    let mut record = serde_json::json!({
        "pushkey": pusher.pushkey,
        "kind": pusher.kind,
        "app_id": pusher.app_id,
        "app_display_name": pusher.app_display_name,
        "device_display_name": pusher.device_display_name,
        "lang": pusher.lang.clone().unwrap_or_else(|| "en".to_owned()),
        "data": pusher.data,
    });
    if let Some(tag) = &pusher.profile_tag {
        record["profile_tag"] = Value::String(tag.clone());
    }
    serde_json::from_value(record).map_err(|e| e.to_string())
}

fn same_json<T: serde::Serialize>(a: &T, b: &T) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
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

    async fn import_device_keys(&self, keys: &SynapseDeviceKeys) -> Result<Imported, TargetError> {
        let id = match self.local_account(&keys.user_id).await? {
            Ok(id) => id,
            Err(skipped) => return Ok(skipped),
        };
        let device: ruma::OwnedDeviceId = keys.device_id.as_str().into();
        if self
            .auth
            .get_device(&id, &device)
            .await
            .map_err(fatal)?
            .is_none()
        {
            return Ok(Imported::Skipped("its device was not copied".to_owned()));
        }
        let mut created = false;
        let mut changed = false;
        if let Some(identity) = &keys.keys {
            match self
                .e2e
                .get_device_keys(&id, &device)
                .await
                .map_err(fatal)?
            {
                Some(here) if here.keys == *identity => {}
                here => {
                    self.e2e
                        .upload_device_keys(&id, &device, identity.clone())
                        .await
                        .map_err(fatal)?;
                    if here.is_some() {
                        changed = true;
                    } else {
                        created = true;
                    }
                }
            }
        }
        // One-time keys: Synapse's that are not here are added (one already here, or already
        // handed out here, is left as it is), then as many as Synapse has handed out since the
        // last pass are claimed here, oldest first.
        let before = self
            .e2e
            .count_one_time_keys(&id, &device)
            .await
            .map_err(fatal)?;
        if !keys.one_time_keys.is_empty() {
            self.e2e
                .upload_one_time_keys(&id, &device, keys.one_time_keys.iter().cloned().collect())
                .await
                .map_err(fatal)?;
        }
        let theirs = Self::synapse_otk_counts(keys);
        let after = self
            .e2e
            .count_one_time_keys(&id, &device)
            .await
            .map_err(fatal)?;
        let mut claimed = false;
        for (algorithm, here) in &after {
            let surplus = here.saturating_sub(theirs.get(algorithm).copied().unwrap_or(0));
            for _ in 0..surplus {
                self.e2e
                    .claim_one_time_key(&id, &device, algorithm)
                    .await
                    .map_err(fatal)?;
                claimed = true;
            }
        }
        if after != before || claimed {
            if keys.keys.is_none() && before.is_empty() {
                created = true;
            } else {
                changed = true;
            }
        }
        // Fallback keys: there is no reading one back, so Synapse's are uploaded again on every
        // pass (an upload replaces the one of its algorithm), and one Synapse has handed out is
        // handed out here too, which is what marks it used.
        if !keys.fallback_keys.is_empty() {
            let unused_before: BTreeSet<String> = self
                .e2e
                .unused_fallback_key_algorithms(&id, &device)
                .await
                .map_err(fatal)?
                .into_iter()
                .collect();
            self.e2e
                .upload_fallback_keys(
                    &id,
                    &device,
                    keys.fallback_keys
                        .iter()
                        .map(|(key_id, key, _)| (key_id.clone(), key.clone()))
                        .collect(),
                )
                .await
                .map_err(fatal)?;
            for (key_id, _, used) in &keys.fallback_keys {
                if *used {
                    let algorithm = key_id.split_once(':').map_or(key_id.as_str(), |(a, _)| a);
                    self.e2e
                        .claim_fallback_key(&id, &device, algorithm)
                        .await
                        .map_err(fatal)?;
                }
            }
            if unused_before != Self::synapse_unused_fallback(keys) && !created {
                changed = true;
            }
        }
        Ok(if created {
            Imported::Created
        } else if changed {
            Imported::Updated
        } else {
            Imported::AlreadyThere
        })
    }

    async fn import_cross_signing(
        &self,
        keys: &SynapseCrossSigning,
    ) -> Result<Imported, TargetError> {
        let id = match self.local_account(&keys.user_id).await? {
            Ok(id) => id,
            Err(skipped) => return Ok(skipped),
        };
        let mut had_any = false;
        let mut changed = false;
        for (kind, key) in [
            (CrossSigningKeyType::Master, &keys.master),
            (CrossSigningKeyType::SelfSigning, &keys.self_signing),
            (CrossSigningKeyType::UserSigning, &keys.user_signing),
        ] {
            let Some(key) = key else { continue };
            let here = self
                .e2e
                .get_cross_signing_key(&id, kind)
                .await
                .map_err(fatal)?;
            had_any |= here.is_some();
            if here.as_ref() == Some(key) {
                continue;
            }
            self.e2e
                .put_cross_signing_key(&id, kind, key.clone())
                .await
                .map_err(fatal)?;
            changed = true;
        }
        if !changed {
            return Ok(Imported::AlreadyThere);
        }
        // As an upload does: other servers and clients learn of the new keys through the
        // device-list stream.
        self.e2e
            .record_device_list_change(&id)
            .await
            .map_err(fatal)?;
        Ok(if had_any {
            Imported::Updated
        } else {
            Imported::Created
        })
    }

    async fn import_backup_version(
        &self,
        version: &SynapseBackupVersion,
    ) -> Result<Imported, TargetError> {
        let id = match self.local_account(&version.user_id).await? {
            Ok(id) => id,
            Err(skipped) => return Ok(skipped),
        };
        if let Some((_, here)) = self
            .e2e
            .get_version(&id, Some(version.version))
            .await
            .map_err(fatal)?
        {
            if here.algorithm != version.algorithm {
                return Err(row(format!(
                    "version {} here is a {} backup, and Synapse's is {}",
                    version.version, here.algorithm, version.algorithm
                )));
            }
            if here.deleted {
                return if version.deleted {
                    Ok(Imported::AlreadyThere)
                } else {
                    Err(row(format!(
                        "version {} was deleted here and not in Synapse",
                        version.version
                    )))
                };
            }
            let mut changed = false;
            if here.auth_data != version.auth_data {
                self.e2e
                    .update_version_auth_data(&id, version.version, version.auth_data.clone())
                    .await
                    .map_err(fatal)?;
                changed = true;
            }
            if version.deleted {
                self.e2e
                    .delete_version(&id, version.version)
                    .await
                    .map_err(fatal)?;
                changed = true;
            }
            return Ok(if changed {
                Imported::Updated
            } else {
                Imported::AlreadyThere
            });
        }
        // Versions are numbered here as in Synapse, from 1 up and never reused: a number Synapse
        // no longer has (it pruned a deleted version) is made and deleted again, so that this
        // version gets its own number.
        loop {
            let made = self
                .e2e
                .create_version(&id, version.algorithm.clone(), version.auth_data.clone())
                .await
                .map_err(fatal)?;
            if made == version.version {
                break;
            }
            self.e2e.delete_version(&id, made).await.map_err(fatal)?;
            if made > version.version {
                return Err(row(format!(
                    "this server already numbers {}'s backups past {}",
                    version.user_id, version.version
                )));
            }
        }
        if version.deleted {
            self.e2e
                .delete_version(&id, version.version)
                .await
                .map_err(fatal)?;
        }
        Ok(Imported::Created)
    }

    async fn import_backup_keys(
        &self,
        user: &str,
        version: u64,
        keys: &[SynapseRoomKey],
    ) -> Result<u64, TargetError> {
        let id = user_id(user)?;
        let mut stored = 0;
        for key in keys {
            let row_here = BackupSessionRow {
                first_message_index: key.first_message_index,
                forwarded_count: key.forwarded_count,
                is_verified: key.is_verified,
                session_data: key.session_data.clone(),
            };
            if self
                .e2e
                .get_session(&id, version, &key.room_id, &key.session_id)
                .await
                .map_err(fatal)?
                .as_ref()
                == Some(&row_here)
            {
                continue;
            }
            if self
                .e2e
                .put_session(&id, version, &key.room_id, &key.session_id, row_here)
                .await
                .map_err(fatal)?
            {
                stored += 1;
            }
        }
        Ok(stored)
    }

    async fn import_push_rules(&self, rules: &SynapsePushRules) -> Result<Imported, TargetError> {
        let id = match self.local_account(&rules.user_id).await? {
            Ok(id) => id,
            Err(skipped) => return Ok(skipped),
        };
        let (ruleset, notes) = Self::ruleset_for(&id, rules);
        for note in &notes {
            tracing::warn!(user_id = %id, "a push rule from Synapse was not carried over: {note}");
        }
        let here = self
            .rulesets
            .store()
            .get_ruleset(&id)
            .await
            .map_err(fatal)?;
        if here.as_ref().is_some_and(|h| same_json(h, &ruleset)) {
            return Ok(Imported::AlreadyThere);
        }
        self.rulesets
            .set_ruleset(&id, &ruleset)
            .await
            .map_err(fatal)?;
        Ok(if here.is_some() {
            Imported::Updated
        } else {
            Imported::Created
        })
    }

    async fn import_pusher(&self, pusher: &SynapsePusher) -> Result<Imported, TargetError> {
        let id = match self.local_account(&pusher.user_id).await? {
            Ok(id) => id,
            Err(skipped) => return Ok(skipped),
        };
        if !pusher.enabled {
            return Ok(Imported::Skipped(
                "turned off in Synapse, and this server keeps only pushers that push".to_owned(),
            ));
        }
        let record = pusher_record(pusher).map_err(|e| row(format!("unreadable pusher: {e}")))?;
        let here = self.pushers.get_pushers(&id).await.map_err(fatal)?;
        let same_ids = |p: &ruma::api::client::push::Pusher| {
            p.ids.app_id == record.ids.app_id && p.ids.pushkey == record.ids.pushkey
        };
        let existing = here.iter().find(|p| same_ids(p));
        if existing.is_some_and(|p| same_json(p, &record)) {
            return Ok(Imported::AlreadyThere);
        }
        let was_there = existing.is_some();
        self.pushers
            .set_pusher(&id, record, None)
            .await
            .map_err(fatal)?;
        Ok(if was_there {
            Imported::Updated
        } else {
            Imported::Created
        })
    }

    async fn import_filter(&self, filter: &SynapseFilter) -> Result<Imported, TargetError> {
        let id = match self.local_account(&filter.user_id).await? {
            Ok(id) => id,
            Err(skipped) => return Ok(skipped),
        };
        let store = self.hub.store();
        let here = store
            .get_filter(&id, &filter.filter_id)
            .await
            .map_err(fatal)?;
        if here.as_ref() == Some(&filter.filter) {
            return Ok(Imported::AlreadyThere);
        }
        store
            .import_filter(&id, &filter.filter_id, filter.filter.clone())
            .await
            .map_err(fatal)?;
        Ok(if here.is_some() {
            Imported::Updated
        } else {
            Imported::Created
        })
    }

    async fn begin_room(&self, room: &SynapseRoom) -> Result<(), TargetError> {
        self.import_handle(room).await.map(|_| ())
    }

    async fn import_remote_join(
        &self,
        room: &SynapseRoom,
        join: &SynapseRemoteJoin,
    ) -> Result<Imported, TargetError> {
        let (handle, version) = self.import_handle(room).await?;
        let join_id = ruma::OwnedEventId::try_from(join.join.event_id.as_str())
            .map_err(|e| row(format!("{:?} is not an event id: {e}", join.join.event_id)))?;
        if handle
            .query(move |a| a.event_by_id(&join_id).is_some())
            .await
        {
            return Ok(Imported::AlreadyThere);
        }
        let parse = |event: &SynapseEvent| -> Result<hs_model::Event, TargetError> {
            let parsed = hs_model::Event::parse(&event.json, version.clone())
                .map_err(|e| row(format!("event {} is unreadable: {e}", event.event_id)))?;
            if parsed.event_id().as_str() != event.event_id {
                return Err(row(format!(
                    "event {} hashes to {}, not to its id",
                    event.event_id,
                    parsed.event_id()
                )));
            }
            Ok(parsed)
        };
        let state = join
            .state
            .iter()
            .map(parse)
            .collect::<Result<Vec<_>, _>>()?;
        let auth_chain = join
            .auth_chain
            .iter()
            .map(parse)
            .collect::<Result<Vec<_>, _>>()?;
        let join_event = parse(&join.join)?;
        match handle
            .import_remote_join(state, auth_chain, join_event)
            .await
        {
            Ok(RemoteEventOutcome::Stored(_) | RemoteEventOutcome::SoftFailed(_)) => {
                Ok(Imported::Created)
            }
            Ok(RemoteEventOutcome::AlreadyKnown) => Ok(Imported::AlreadyThere),
            Err(RoomError::Store(e)) => Err(fatal(e)),
            Err(e) => {
                if let Ok(room_id) = ruma::OwnedRoomId::try_from(room.room_id.as_str()) {
                    self.rooms.discard_import_shell(&room_id, &handle).await;
                }
                Err(row(format!(
                    "its join {} could not be taken as where it starts here: {e}",
                    join.join.event_id
                )))
            }
        }
    }

    async fn import_room_events(
        &self,
        room: &SynapseRoom,
        events: &[SynapseEvent],
    ) -> Result<RoomOutcome, TargetError> {
        let (handle, version) = self.import_handle(room).await?;
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
                Ok(RemoteEventOutcome::Stored(_) | RemoteEventOutcome::SoftFailed(_)) => {
                    outcome.stored += 1;
                }
                Ok(RemoteEventOutcome::AlreadyKnown) => outcome.already_there += 1,
                Err(RoomError::MissingAncestors(_)) => outcome.waiting.push(event.event_id.clone()),
                Err(RoomError::Store(e)) => return Err(fatal(e)),
                Err(e) => outcome
                    .refused
                    .push((event.event_id.clone(), e.to_string())),
            }
        }
        Ok(outcome)
    }

    async fn finish_room(&self, room: &SynapseRoom) -> Result<RoomOutcome, TargetError> {
        let (handle, _) = self.import_handle(room).await?;
        let room_id = ruma::OwnedRoomId::try_from(room.room_id.as_str())
            .map_err(|e| row(format!("{:?} is not a room id: {e}", room.room_id)))?;
        let held = handle.query(|a| a.head_update().is_some()).await;
        if !held {
            self.rooms.discard_import_shell(&room_id, &handle).await;
            return Err(row(
                "none of its events could be stored, its m.room.create first (see the refusals \
                 above)",
            ));
        }
        let mut outcome = RoomOutcome::default();
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

    async fn import_receipt(&self, receipt: &SynapseReceipt) -> Result<Imported, TargetError> {
        let Some(kind) = ReceiptKind::parse(&receipt.receipt_type) else {
            return Ok(Imported::Skipped(format!(
                "a {} receipt is not kept here",
                receipt.receipt_type
            )));
        };
        let user = user_id(&receipt.user_id)?;
        let room_id = ruma::OwnedRoomId::try_from(receipt.room_id.as_str())
            .map_err(|e| row(format!("{:?} is not a room id: {e}", receipt.room_id)))?;
        let event_id = ruma::OwnedEventId::try_from(receipt.event_id.as_str())
            .map_err(|e| row(format!("{:?} is not an event id: {e}", receipt.event_id)))?;
        match self.rooms.get_or_load(&room_id).await {
            Ok(_) => {}
            Err(RoomError::RoomNotFound(_)) => {
                return Ok(Imported::Skipped("its room was not copied".to_owned()));
            }
            Err(e) => return Err(fatal(e)),
        }
        let check = self.verify_receipt(receipt).await?;
        if check == Check::Same {
            return Ok(Imported::AlreadyThere);
        }
        self.hub
            .import_receipt(&room_id, &user, kind, event_id, receipt.ts)
            .await
            .map_err(row)?;
        Ok(if check == Check::Missing {
            Imported::Created
        } else {
            Imported::Updated
        })
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

    async fn room_state(&self, room_id_raw: &str) -> Result<Option<CurrentState>, TargetError> {
        let Ok(room_id) = ruma::OwnedRoomId::try_from(room_id_raw) else {
            return Ok(None);
        };
        let handle = match self.rooms.get_or_load(&room_id).await {
            Ok(handle) => handle,
            Err(RoomError::RoomNotFound(_)) => return Ok(None),
            Err(e) => return Err(fatal(e)),
        };
        let state = handle
            .query(|a| -> Result<CurrentState, RoomError> {
                Ok(a.full_state()?
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
                    .collect())
            })
            .await
            .map_err(fatal)?;
        Ok(Some(state))
    }

    async fn missing_events(
        &self,
        room_id_raw: &str,
        event_ids: &[String],
    ) -> Result<Vec<String>, TargetError> {
        let Ok(room_id) = ruma::OwnedRoomId::try_from(room_id_raw) else {
            return Ok(event_ids.to_vec());
        };
        let handle = match self.rooms.get_or_load(&room_id).await {
            Ok(handle) => handle,
            Err(RoomError::RoomNotFound(_)) => return Ok(event_ids.to_vec()),
            Err(e) => return Err(fatal(e)),
        };
        let ids = event_ids.to_vec();
        Ok(handle
            .query(move |a| {
                ids.into_iter()
                    .filter(|id| {
                        ruma::EventId::parse(id.as_str())
                            .map_or(true, |parsed| a.event_by_id(&parsed).is_none())
                    })
                    .collect()
            })
            .await)
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

    async fn verify_device_keys(&self, keys: &SynapseDeviceKeys) -> Result<Check, TargetError> {
        let id = user_id(&keys.user_id)?;
        let device: ruma::OwnedDeviceId = keys.device_id.as_str().into();
        let here = self
            .e2e
            .get_device_keys(&id, &device)
            .await
            .map_err(fatal)?;
        let mut differs = Vec::new();
        match (&keys.keys, here) {
            (Some(_), None) => return Ok(Check::Missing),
            (Some(theirs), Some(ours)) if *theirs != ours.keys => differs.push("identity keys"),
            _ => {}
        }
        let counts = self
            .e2e
            .count_one_time_keys(&id, &device)
            .await
            .map_err(fatal)?;
        if counts != Self::synapse_otk_counts(keys) {
            differs.push("one-time key counts");
        }
        let unused: BTreeSet<String> = self
            .e2e
            .unused_fallback_key_algorithms(&id, &device)
            .await
            .map_err(fatal)?
            .into_iter()
            .collect();
        if unused != Self::synapse_unused_fallback(keys) {
            differs.push("unused fallback keys");
        }
        Ok(if differs.is_empty() {
            Check::Same
        } else {
            Check::Differs(format!("{} differ", differs.join(", ")))
        })
    }

    async fn verify_cross_signing(&self, keys: &SynapseCrossSigning) -> Result<Check, TargetError> {
        let id = user_id(&keys.user_id)?;
        let mut missing = 0;
        let mut differs = Vec::new();
        for (kind, key) in [
            (CrossSigningKeyType::Master, &keys.master),
            (CrossSigningKeyType::SelfSigning, &keys.self_signing),
            (CrossSigningKeyType::UserSigning, &keys.user_signing),
        ] {
            let Some(key) = key else { continue };
            match self
                .e2e
                .get_cross_signing_key(&id, kind)
                .await
                .map_err(fatal)?
            {
                None => missing += 1,
                Some(here) if here != *key => differs.push(kind.as_str()),
                Some(_) => {}
            }
        }
        Ok(if missing > 0 && differs.is_empty() {
            Check::Missing
        } else if missing > 0 || !differs.is_empty() {
            Check::Differs(format!(
                "{missing} keys missing, {} differ",
                if differs.is_empty() {
                    "none".to_owned()
                } else {
                    differs.join(", ")
                }
            ))
        } else {
            Check::Same
        })
    }

    async fn verify_backup_version(
        &self,
        version: &SynapseBackupVersion,
        key_count: u64,
    ) -> Result<Check, TargetError> {
        let id = user_id(&version.user_id)?;
        let Some((_, here)) = self
            .e2e
            .get_version(&id, Some(version.version))
            .await
            .map_err(fatal)?
        else {
            return Ok(Check::Missing);
        };
        let mut differs = Vec::new();
        if here.deleted != version.deleted {
            differs.push("deleted or not".to_owned());
        }
        if !here.deleted && here.auth_data != version.auth_data {
            differs.push("auth_data".to_owned());
        }
        if here.algorithm != version.algorithm {
            differs.push("algorithm".to_owned());
        }
        if !here.deleted && here.count != key_count {
            differs.push(format!(
                "{} room keys here, {key_count} in Synapse",
                here.count
            ));
        }
        Ok(if differs.is_empty() {
            Check::Same
        } else {
            Check::Differs(differs.join(", "))
        })
    }

    async fn verify_push_rules(&self, rules: &SynapsePushRules) -> Result<Check, TargetError> {
        let id = user_id(&rules.user_id)?;
        let Some(here) = self
            .rulesets
            .store()
            .get_ruleset(&id)
            .await
            .map_err(fatal)?
        else {
            return Ok(Check::Missing);
        };
        let (theirs, _) = Self::ruleset_for(&id, rules);
        Ok(if same_json(&here, &theirs) {
            Check::Same
        } else {
            Check::Differs("the rules differ".to_owned())
        })
    }

    async fn verify_pusher(&self, pusher: &SynapsePusher) -> Result<Check, TargetError> {
        let id = user_id(&pusher.user_id)?;
        let here = self.pushers.get_pushers(&id).await.map_err(fatal)?;
        let found = here
            .iter()
            .find(|p| p.ids.app_id == pusher.app_id && p.ids.pushkey == pusher.pushkey);
        if !pusher.enabled {
            return Ok(if found.is_none() {
                Check::Same
            } else {
                Check::Differs("turned off in Synapse, and pushing here".to_owned())
            });
        }
        let Some(found) = found else {
            return Ok(Check::Missing);
        };
        let theirs = pusher_record(pusher).map_err(row)?;
        Ok(if same_json(found, &theirs) {
            Check::Same
        } else {
            Check::Differs("the pusher differs".to_owned())
        })
    }

    async fn verify_filter(&self, filter: &SynapseFilter) -> Result<Check, TargetError> {
        let id = user_id(&filter.user_id)?;
        Ok(
            match self
                .hub
                .store()
                .get_filter(&id, &filter.filter_id)
                .await
                .map_err(fatal)?
            {
                None => Check::Missing,
                Some(here) if here == filter.filter => Check::Same,
                Some(_) => Check::Differs("the filter differs".to_owned()),
            },
        )
    }

    async fn verify_receipt(&self, receipt: &SynapseReceipt) -> Result<Check, TargetError> {
        let user = user_id(&receipt.user_id)?;
        let Ok(room_id) = ruma::OwnedRoomId::try_from(receipt.room_id.as_str()) else {
            return Ok(Check::Missing);
        };
        let (content, _) = self.hub.receipt_content_for(&room_id, &user).await;
        let mine = content
            .as_object()
            .into_iter()
            .flatten()
            .find_map(|(event_id, by_type)| {
                by_type
                    .get(&receipt.receipt_type)
                    .and_then(|users| users.get(user.as_str()))
                    .map(|r| (event_id.clone(), r.get("ts").and_then(Value::as_u64)))
            });
        Ok(match mine {
            None => Check::Missing,
            Some((event_id, ts)) if event_id == receipt.event_id && ts == Some(receipt.ts) => {
                Check::Same
            }
            Some((event_id, _)) => Check::Differs(format!("here it is at {event_id}")),
        })
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
/// status, 0 for the others); and the room copy's throughput: `hs_migration_events_read_total`,
/// `hs_migration_events_stored_total`, `hs_migration_event_bytes_read_total`,
/// `hs_migration_room_seconds` (a histogram, one observation per room) and
/// `hs_migration_peak_rss_bytes` (this process's peak memory when the last room was done).
#[derive(Clone)]
pub struct MigrationMetrics {
    copied: Family<StreamLabels, Gauge>,
    skipped: Family<StreamLabels, Gauge>,
    failed: Family<StreamLabels, Gauge>,
    source: Family<StreamLabels, Gauge>,
    status: Family<StatusLabels, Gauge>,
    events_read: Counter,
    events_stored: Counter,
    event_bytes: Counter,
    room_seconds: Histogram,
    peak_rss: Gauge,
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
            events_read: Counter::default(),
            events_stored: Counter::default(),
            event_bytes: Counter::default(),
            room_seconds: Histogram::new(exponential_buckets(0.01, 4.0, 10)),
            peak_rss: Gauge::default(),
        };
        metrics.with_registry(|registry| {
            registry.register(
                "hs_migration_events_read",
                "Events of rooms read from Synapse by the migration",
                this.events_read.clone(),
            );
            registry.register(
                "hs_migration_events_stored",
                "Events of rooms newly stored here by the migration",
                this.events_stored.clone(),
            );
            registry.register(
                "hs_migration_event_bytes_read",
                "Bytes of events read from Synapse by the migration, as Synapse stored them",
                this.event_bytes.clone(),
            );
            registry.register(
                "hs_migration_room_seconds",
                "How long the migration took to copy each room",
                this.room_seconds.clone(),
            );
            registry.register(
                "hs_migration_peak_rss_bytes",
                "This process's peak resident memory when the migration last finished a room",
                this.peak_rss.clone(),
            );
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
    fn room_copied(&self, stats: &RoomStats) {
        self.events_read.inc_by(stats.events_read);
        self.events_stored.inc_by(stats.events_stored);
        self.event_bytes.inc_by(stats.bytes);
        self.room_seconds.observe(stats.elapsed.as_secs_f64());
        if let Some(peak) = stats.peak_rss_bytes {
            self.peak_rss.set(i64::try_from(peak).unwrap_or(i64::MAX));
        }
    }

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
    /// End-to-end keys, push rules and pushers.
    pub sessions: SessionStores<B>,
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
        parts.sessions,
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
