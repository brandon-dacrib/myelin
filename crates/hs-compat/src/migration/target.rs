//! Where the importer writes: [`MigrationTarget`], implemented by `hs-cli` over this server's
//! own stores (accounts, devices and tokens in `hs-auth`; account data, filters and receipts in
//! `hs-user`; end-to-end keys and key backups in `hs-e2e`; push rules and pushers in `hs-push`;
//! rooms in `hs-room`; media in `hs-media`).
//!
//! Every write is idempotent: importing a row that is already here answers
//! [`Imported::AlreadyThere`] (or [`Imported::Updated`], when Synapse's copy has changed since),
//! which is what lets a copy stop anywhere and resume, and a cutover make a final pass over
//! everything.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

use super::model::{
    SynapseAccessToken, SynapseAccountData, SynapseBackupVersion, SynapseCrossSigning,
    SynapseDevice, SynapseDeviceKeys, SynapseEvent, SynapseFilter, SynapseMedia, SynapsePushRules,
    SynapsePusher, SynapseReceipt, SynapseRemoteJoin, SynapseRoom, SynapseRoomKey, SynapseUser,
};

/// What importing one row did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Imported {
    /// It was not here, and now is.
    Created,
    /// It was here, and was brought up to date.
    Updated,
    /// It was here already, as it is in Synapse.
    AlreadyThere,
    /// Deliberately not copied, and why (logged as a warning).
    Skipped(String),
}

/// A failure to import. A row failure is logged and counted and the copy goes on; a fatal one
/// (this server's database is unreachable) stops the copy, which can be resumed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TargetError {
    /// Stop the copy rather than go on to the next row.
    pub fatal: bool,
    /// What went wrong.
    pub message: String,
}

impl TargetError {
    /// A failure of this one row.
    #[must_use]
    pub fn row(message: impl Into<String>) -> Self {
        Self {
            fatal: false,
            message: message.into(),
        }
    }

    /// A failure no later row would escape either.
    #[must_use]
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            fatal: true,
            message: message.into(),
        }
    }
}

/// What importing a page of a room's events, or finishing the room, did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomOutcome {
    /// Events newly stored.
    pub stored: u64,
    /// Events already here.
    pub already_there: u64,
    /// Events this server's authorization refused, with why.
    pub refused: Vec<(String, String)>,
    /// Events not stored yet because they cite an event this server does not hold yet: the
    /// importer offers them again once more of the room is in, and refuses them only if what
    /// they cite never arrives.
    pub waiting: Vec<String>,
    /// Redactions applied.
    pub redactions: u64,
    /// Aliases created.
    pub aliases: u64,
}

impl RoomOutcome {
    /// Adds `other`'s counts and lists to this one's.
    pub fn absorb(&mut self, other: RoomOutcome) {
        self.stored += other.stored;
        self.already_there += other.already_there;
        self.refused.extend(other.refused);
        self.waiting.extend(other.waiting);
        self.redactions += other.redactions;
        self.aliases += other.aliases;
    }
}

/// What verification found for one row: here as in Synapse, not here, or here but different.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Here, and the same as in Synapse.
    Same,
    /// Not here.
    Missing,
    /// Here, but different; what differs.
    Differs(String),
}

/// An account as this server holds it, for verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetUser {
    /// The stored password hash.
    pub password_hash: Option<String>,
    /// Display name.
    pub displayname: Option<String>,
    /// Avatar.
    pub avatar_url: Option<String>,
    /// Administrator.
    pub admin: bool,
    /// Deactivated.
    pub deactivated: bool,
}

/// A room's current state: `(type, state_key) -> event_id`.
pub type CurrentState = BTreeMap<(String, String), String>;

/// A media item as this server holds it, for verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetMedia {
    /// Its content type.
    pub content_type: String,
    /// Its bytes, when they are here.
    pub bytes: Option<Vec<u8>>,
}

/// This server's stores, as the importer writes and verifies them.
#[async_trait]
pub trait MigrationTarget: Send + Sync + 'static {
    /// This server's name. Only a Synapse for the same name can be migrated into it.
    fn server_name(&self) -> &str;

    /// Imports an account (creating it, or updating its hash, flags and profile).
    async fn import_user(&self, user: &SynapseUser) -> Result<Imported, TargetError>;
    /// Imports a device.
    async fn import_device(&self, device: &SynapseDevice) -> Result<Imported, TargetError>;
    /// Imports an access token, so that the client holding it stays signed in.
    async fn import_access_token(
        &self,
        token: &SynapseAccessToken,
    ) -> Result<Imported, TargetError>;
    /// Imports one piece of account data.
    async fn import_account_data(&self, data: &SynapseAccountData)
    -> Result<Imported, TargetError>;
    /// Imports one device's end-to-end keys: its identity keys, and its one-time and fallback
    /// keys made the same as Synapse's.
    async fn import_device_keys(&self, keys: &SynapseDeviceKeys) -> Result<Imported, TargetError>;
    /// Imports one account's cross-signing keys.
    async fn import_cross_signing(
        &self,
        keys: &SynapseCrossSigning,
    ) -> Result<Imported, TargetError>;
    /// Imports one key backup version (under the same number; a deleted one stays deleted).
    async fn import_backup_version(
        &self,
        version: &SynapseBackupVersion,
    ) -> Result<Imported, TargetError>;
    /// Imports room keys into a backup version [`MigrationTarget::import_backup_version`]
    /// imported; how many were stored (new, or better than the one here).
    async fn import_backup_keys(
        &self,
        user_id: &str,
        version: u64,
        keys: &[SynapseRoomKey],
    ) -> Result<u64, TargetError>;
    /// Imports one account's push rules.
    async fn import_push_rules(&self, rules: &SynapsePushRules) -> Result<Imported, TargetError>;
    /// Imports a pusher.
    async fn import_pusher(&self, pusher: &SynapsePusher) -> Result<Imported, TargetError>;
    /// Imports a sync filter, under its id.
    async fn import_filter(&self, filter: &SynapseFilter) -> Result<Imported, TargetError>;
    /// Makes ready to import a room: [`MigrationTarget::import_room_events`] follows, a page at
    /// a time, then [`MigrationTarget::finish_room`].
    async fn begin_room(&self, room: &SynapseRoom) -> Result<(), TargetError>;
    /// Makes a room this server's users joined over federation held here, as the join made it
    /// held in Synapse: from the join, the state before it and that state's auth chain (what
    /// the resident server's `send_join` answered). Its history after the join follows through
    /// [`MigrationTarget::begin_room`] and the rest, as a room made here does.
    async fn import_remote_join(
        &self,
        room: &SynapseRoom,
        join: &SynapseRemoteJoin,
    ) -> Result<Imported, TargetError>;
    /// Imports a page of a room's events, in an order in which each one's ancestors come first
    /// (an event citing one that is not here yet is answered in [`RoomOutcome::waiting`]).
    async fn import_room_events(
        &self,
        room: &SynapseRoom,
        events: &[SynapseEvent],
    ) -> Result<RoomOutcome, TargetError>;
    /// Finishes a room once all its events are in: `room.redactions`, `room.aliases` and its
    /// directory listing, and its members see it in `/sync`. A room none of whose events could
    /// be stored is not left behind, and is a failure of the room.
    async fn finish_room(&self, room: &SynapseRoom) -> Result<RoomOutcome, TargetError>;
    /// Imports a read receipt, into a room imported before it.
    async fn import_receipt(&self, receipt: &SynapseReceipt) -> Result<Imported, TargetError>;
    /// Imports a local media item, with its bytes when the media store is mounted.
    async fn import_media(
        &self,
        media: &SynapseMedia,
        bytes: Option<Vec<u8>>,
    ) -> Result<Imported, TargetError>;

    /// An account, for verification.
    async fn user(&self, user_id: &str) -> Result<Option<TargetUser>, TargetError>;
    /// A device's display name (`Some(None)` for a device without one), for verification.
    async fn device(
        &self,
        user_id: &str,
        device_id: &str,
    ) -> Result<Option<Option<String>>, TargetError>;
    /// Who an access token signs in (`(user id, device id)`), for verification.
    async fn access_token(
        &self,
        token: &str,
    ) -> Result<Option<(String, Option<String>)>, TargetError>;
    /// One piece of account data, for verification.
    async fn account_data(
        &self,
        user_id: &str,
        room_id: Option<&str>,
        data_type: &str,
    ) -> Result<Option<Value>, TargetError>;
    /// A room's current state, or `None` when the room is not here, for verification.
    async fn room_state(&self, room_id: &str) -> Result<Option<CurrentState>, TargetError>;
    /// Which of `event_ids` (all of one room) are not here, for verification.
    async fn missing_events(
        &self,
        room_id: &str,
        event_ids: &[String],
    ) -> Result<Vec<String>, TargetError>;
    /// A local media item, for verification.
    async fn media(&self, media_id: &str) -> Result<Option<TargetMedia>, TargetError>;
    /// Whether one device's end-to-end keys are here as in Synapse (identity keys the same,
    /// as many one-time keys of each algorithm, the fallback keys there).
    async fn verify_device_keys(&self, keys: &SynapseDeviceKeys) -> Result<Check, TargetError>;
    /// Whether one account's cross-signing keys are here as in Synapse.
    async fn verify_cross_signing(&self, keys: &SynapseCrossSigning) -> Result<Check, TargetError>;
    /// Whether a backup version is here as in Synapse, holding `key_count` room keys.
    async fn verify_backup_version(
        &self,
        version: &SynapseBackupVersion,
        key_count: u64,
    ) -> Result<Check, TargetError>;
    /// Whether one account's push rules are here as in Synapse.
    async fn verify_push_rules(&self, rules: &SynapsePushRules) -> Result<Check, TargetError>;
    /// Whether a pusher is here as in Synapse.
    async fn verify_pusher(&self, pusher: &SynapsePusher) -> Result<Check, TargetError>;
    /// Whether a filter is here as in Synapse.
    async fn verify_filter(&self, filter: &SynapseFilter) -> Result<Check, TargetError>;
    /// Whether a receipt is here as in Synapse.
    async fn verify_receipt(&self, receipt: &SynapseReceipt) -> Result<Check, TargetError>;
}
