//! Where the importer writes: [`MigrationTarget`], implemented by `hs-cli` over this server's
//! own stores (accounts, devices and tokens in `hs-auth`, account data in `hs-user`, rooms in
//! `hs-room`, media in `hs-media`).
//!
//! Every write is idempotent: importing a row that is already here answers
//! [`Imported::AlreadyThere`] (or [`Imported::Updated`], when Synapse's copy has changed since),
//! which is what lets a copy stop anywhere and resume, and a cutover make a final pass over
//! everything.

use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use serde_json::Value;

use super::model::{
    SynapseAccessToken, SynapseAccountData, SynapseDevice, SynapseEvent, SynapseMedia, SynapseRoom,
    SynapseUser,
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

/// What importing a room did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomOutcome {
    /// Events newly stored.
    pub stored: u64,
    /// Events already here.
    pub already_there: u64,
    /// Events this server's authorization refused, with why.
    pub refused: Vec<(String, String)>,
    /// Redactions applied.
    pub redactions: u64,
    /// Aliases created.
    pub aliases: u64,
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

/// A room as this server holds it, for verification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetRoom {
    /// Every event id held.
    pub event_ids: HashSet<String>,
    /// Current state: `(type, state_key) -> event_id`.
    pub current_state: BTreeMap<(String, String), String>,
}

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
    /// Imports a room: `events` (already in an order in which each one's ancestors come first,
    /// see [`crate::migration::order`]), then `room.redactions`, `room.aliases` and its
    /// directory listing. Its members see it in `/sync` once it is in.
    async fn import_room(
        &self,
        room: &SynapseRoom,
        events: &[&SynapseEvent],
    ) -> Result<RoomOutcome, TargetError>;
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
    /// A room, for verification.
    async fn room(&self, room_id: &str) -> Result<Option<TargetRoom>, TargetError>;
    /// A local media item, for verification.
    async fn media(&self, media_id: &str) -> Result<Option<TargetMedia>, TargetError>;
}
