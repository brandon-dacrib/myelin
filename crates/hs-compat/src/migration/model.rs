//! What the importer reads from Synapse, what it keeps about a migration, and what a
//! verification finds.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The streams a migration copies, in the order it copies them. Each one's rows only refer to
/// rows of the streams before it (a device to its user, a room's events to its members), so a
/// stream is never copied before what it depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    /// Accounts, with their password hashes, flags and profiles (`users`, `profiles`).
    Users,
    /// Devices (`devices`).
    Devices,
    /// Access tokens (`access_tokens`), so that signed-in clients stay signed in.
    AccessTokens,
    /// Global and per-room account data, and room tags (`account_data`, `room_account_data`,
    /// `room_tags`).
    AccountData,
    /// Rooms: every event of each room, its aliases and its directory listing (`rooms`,
    /// `events`, `event_json`, `redactions`, `room_aliases`).
    Rooms,
    /// Local media: the records and, when the media store is mounted, the files
    /// (`local_media_repository`, `media_store_path/local_content`).
    Media,
}

impl Stream {
    /// Every stream, in copy order.
    pub const ALL: [Stream; 6] = [
        Stream::Users,
        Stream::Devices,
        Stream::AccessTokens,
        Stream::AccountData,
        Stream::Rooms,
        Stream::Media,
    ];

    /// The wire name (`MigrationStatus.streams[].name`, `MigrationLogEntry.stream`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Users => "users",
            Stream::Devices => "devices",
            Stream::AccessTokens => "access_tokens",
            Stream::AccountData => "account_data",
            Stream::Rooms => "rooms",
            Stream::Media => "media",
        }
    }

    /// The stream with this wire name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Stream> {
        Stream::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

/// A Synapse account (`users` joined with `profiles`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseUser {
    /// `@localpart:server`.
    pub user_id: String,
    /// Synapse's bcrypt hash (`$2b$...`), or `None` for an account with no password (SSO only,
    /// or deactivated).
    pub password_hash: Option<String>,
    /// When it was created, in milliseconds (Synapse keeps seconds).
    pub created_at_ms: u64,
    /// A server administrator.
    pub admin: bool,
    /// A guest account.
    pub guest: bool,
    /// Deactivated.
    pub deactivated: bool,
    /// Shadow-banned.
    pub shadow_banned: bool,
    /// Locked (Synapse 1.93 and later).
    pub locked: bool,
    /// The appservice that registered it, if one did.
    pub appservice_id: Option<String>,
    /// `bot` or `support`, or `None` for an ordinary account.
    pub user_type: Option<String>,
    /// The profile's display name.
    pub displayname: Option<String>,
    /// The profile's avatar (`mxc://`).
    pub avatar_url: Option<String>,
}

/// A Synapse device (`devices`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseDevice {
    /// Whose device it is.
    pub user_id: String,
    /// The device id.
    pub device_id: String,
    /// Its display name.
    pub display_name: Option<String>,
    /// When it was last seen, in milliseconds.
    pub last_seen_ms: Option<u64>,
    /// The address it was last seen from.
    pub last_seen_ip: Option<String>,
    /// A device Synapse keeps for its own purposes (a cross-signing key's pseudo-device), never
    /// shown to clients. Not copied.
    pub hidden: bool,
}

/// A Synapse access token (`access_tokens`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseAccessToken {
    /// Synapse's row id, which only grows: the stream's checkpoint.
    pub id: i64,
    /// Whose token it is.
    pub user_id: String,
    /// The device it belongs to.
    pub device_id: Option<String>,
    /// The token itself. Never logged.
    pub token: String,
    /// When it stops working, in milliseconds; `None` for never.
    pub valid_until_ms: Option<u64>,
    /// Set on a token an administrator made to act as `user_id` ("login as"). Not copied.
    pub puppets_user_id: Option<String>,
}

impl std::fmt::Debug for SynapseAccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SynapseAccessToken")
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("token", &"<redacted>")
            .field("valid_until_ms", &self.valid_until_ms)
            .field("puppets_user_id", &self.puppets_user_id)
            .finish()
    }
}

/// One piece of account data: global when `room_id` is `None`. Room tags arrive as one `m.tag`
/// per room, shaped as clients read it (`{"tags": {...}}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseAccountData {
    /// Whose it is.
    pub user_id: String,
    /// The room it is for, or `None` for global account data.
    pub room_id: Option<String>,
    /// The event type (`m.direct`, `m.tag`, ...).
    pub data_type: String,
    /// The content.
    pub content: Value,
}

/// One event of a room, as Synapse stores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseEvent {
    /// Its id.
    pub event_id: String,
    /// The event as Synapse stored it (`event_json.json`): the PDU, without `unsigned`.
    pub json: Value,
    /// Its depth.
    pub depth: i64,
    /// Synapse's `stream_ordering`: the order it was stored in.
    pub stream_ordering: i64,
    /// Synapse holds it without holding its place in the room's history (state or auth events
    /// fetched for a join over federation, an invite from another server).
    pub outlier: bool,
    /// Synapse rejected it.
    pub rejected: bool,
}

/// A room, with everything the importer copies of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseRoom {
    /// Its id.
    pub room_id: String,
    /// Its room version (`rooms.room_version`; Synapse leaves it null for very old rooms, which
    /// are version 1).
    pub room_version: String,
    /// Listed in the room directory.
    pub is_public: bool,
    /// Its aliases, each with who made it.
    pub aliases: Vec<(String, Option<String>)>,
    /// Its events, in the order Synapse stored them.
    pub events: Vec<SynapseEvent>,
    /// `(redaction event id, redacted event id)` for each redaction Synapse accepted.
    pub redactions: Vec<(String, String)>,
}

/// A local media item (`local_media_repository`), and where its file is in the media store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseMedia {
    /// Its media id (the path of `mxc://server/<media_id>`).
    pub media_id: String,
    /// Its content type.
    pub content_type: Option<String>,
    /// Its size in bytes.
    pub length: Option<u64>,
    /// When it was uploaded, in milliseconds.
    pub created_ms: u64,
    /// The file name it was uploaded with.
    pub upload_name: Option<String>,
    /// Who uploaded it.
    pub uploader: Option<String>,
    /// Who quarantined it, if anybody did.
    pub quarantined_by: Option<String>,
    /// Protected from quarantine.
    pub safe_from_quarantine: bool,
}

/// Where a migration is. The OpenAPI `MigrationStatus.status` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Nothing started.
    #[default]
    Idle,
    /// Copying.
    Copying,
    /// Stopped by an administrator part way; resumes where it stopped.
    Paused,
    /// Everything copied once; can be verified, and cut over to.
    ReadyForCutover,
    /// The final pass and verification of a cutover are running.
    CuttingOver,
    /// A verification is running.
    Verifying,
    /// Cut over: this server is the one in service, and the migration is finished.
    Completed,
    /// The copy stopped on an error; it can be started again and resumes where it stopped.
    Failed,
    /// Abandoned by an administrator. Synapse was never written to; what was copied stays.
    Aborted,
}

impl Phase {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Copying => "copying",
            Phase::Paused => "paused",
            Phase::ReadyForCutover => "ready_for_cutover",
            Phase::CuttingOver => "cutting_over",
            Phase::Verifying => "verifying",
            Phase::Completed => "completed",
            Phase::Failed => "failed",
            Phase::Aborted => "aborted",
        }
    }

    /// Every phase, for metrics that have one series per phase.
    pub const ALL: [Phase; 9] = [
        Phase::Idle,
        Phase::Copying,
        Phase::Paused,
        Phase::ReadyForCutover,
        Phase::CuttingOver,
        Phase::Verifying,
        Phase::Completed,
        Phase::Failed,
        Phase::Aborted,
    ];

    /// Whether work is running in this phase (a restart must pick it up again).
    #[must_use]
    pub fn is_running(self) -> bool {
        matches!(self, Phase::Copying | Phase::CuttingOver | Phase::Verifying)
    }
}

/// How far one stream has got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamProgress {
    /// Which stream.
    pub stream: Stream,
    /// Rows now present here (copied, or found already copied by an earlier pass).
    pub copied: u64,
    /// Rows deliberately not copied (each one is in the log, with why).
    pub skipped: u64,
    /// Rows that could not be copied (each one is in the log, with the error).
    pub failed: u64,
    /// Rows in Synapse, counted when the stream started; `None` before that.
    pub total: Option<u64>,
    /// The key of the last row handled; the next batch starts after it.
    pub checkpoint: Option<String>,
    /// Every row has been read.
    pub done: bool,
    /// Rows per second over the stream's last run.
    pub rate_per_second: f64,
    /// When the stream's current run started (milliseconds), for the rate.
    #[serde(default)]
    pub run_started_ms: Option<u64>,
    /// Rows handled in the current run, for the rate.
    #[serde(default)]
    pub run_rows: u64,
}

impl StreamProgress {
    /// A stream nothing has been done for.
    #[must_use]
    pub fn new(stream: Stream) -> Self {
        Self {
            stream,
            copied: 0,
            skipped: 0,
            failed: 0,
            total: None,
            checkpoint: None,
            done: false,
            rate_per_second: 0.0,
            run_started_ms: None,
            run_rows: 0,
        }
    }
}

/// Everything durable about the one migration a server has: its phase, where each stream has
/// got, and the last verification. Kept by a [`crate::migration::MigrationStore`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MigrationRecord {
    /// Where it is.
    pub phase: Phase,
    /// The source, described without its password (`postgresql://user@host:port/db`).
    pub source: Option<String>,
    /// The configuration pointer the source was read from (`/migration/synapse`).
    pub source_ref: Option<String>,
    /// Bumped by every start, resume and cutover. Work carries the run it was started for and
    /// writes nothing once a newer one exists, so a copy that was stopped cannot overwrite what
    /// replaced it.
    pub run: u64,
    /// The task running the current work, if any.
    pub task_id: Option<String>,
    /// Per stream, in copy order. Empty until the first start.
    pub streams: Vec<StreamProgress>,
    /// Why the last run failed, most recent last (kept short).
    pub errors: Vec<String>,
    /// When the first start happened (RFC 3339).
    pub started_at: Option<String>,
    /// Who started it.
    pub started_by: Option<String>,
    /// Last change (RFC 3339).
    pub updated_at: Option<String>,
    /// When the cutover finished (RFC 3339).
    pub completed_at: Option<String>,
    /// Who cut over.
    pub cutover_by: Option<String>,
    /// The phase a verification returns to when it ends.
    #[serde(default)]
    pub resume_phase: Option<Phase>,
    /// The last verification's findings.
    pub verification: Option<VerificationReport>,
}

impl MigrationRecord {
    /// This stream's progress, created if missing.
    pub fn stream_mut(&mut self, stream: Stream) -> &mut StreamProgress {
        if let Some(i) = self.streams.iter().position(|s| s.stream == stream) {
            return &mut self.streams[i];
        }
        self.streams.push(StreamProgress::new(stream));
        self.streams.sort_by_key(|s| s.stream);
        let i = self
            .streams
            .iter()
            .position(|s| s.stream == stream)
            .unwrap_or(0);
        &mut self.streams[i]
    }

    /// This stream's progress, if it has any.
    #[must_use]
    pub fn stream(&self, stream: Stream) -> Option<&StreamProgress> {
        self.streams.iter().find(|s| s.stream == stream)
    }

    /// Keeps the most recent errors only.
    pub fn push_error(&mut self, error: impl Into<String>) {
        self.errors.push(error.into());
        let excess = self.errors.len().saturating_sub(20);
        self.errors.drain(..excess);
    }

    /// How long the rest should take, from each unfinished stream's rate; `None` when nothing
    /// is known yet.
    #[must_use]
    pub fn estimated_remaining_ms(&self) -> Option<u64> {
        let mut total_ms = 0.0_f64;
        let mut known = false;
        for s in &self.streams {
            if s.done {
                continue;
            }
            let Some(total) = s.total else { continue };
            if s.rate_per_second <= 0.0 {
                continue;
            }
            let left = total.saturating_sub(s.copied + s.skipped + s.failed);
            #[allow(clippy::cast_precision_loss)]
            {
                total_ms += left as f64 / s.rate_per_second * 1000.0;
            }
            known = true;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        known.then_some(total_ms as u64)
    }
}

/// How serious a log entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Something happened.
    Info,
    /// Something was not copied, on purpose, and why.
    Warning,
    /// Something could not be copied.
    Error,
}

/// One entry of the migration's log (the OpenAPI `MigrationLogEntry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// When (RFC 3339).
    pub recorded_at: String,
    /// The stream it is about, or `migration` for the migration as a whole.
    pub stream: String,
    /// What happened.
    pub message: String,
    /// How serious.
    pub level: LogLevel,
}

/// What a verification found for one stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamVerification {
    /// The stream's wire name.
    pub name: String,
    /// Rows in Synapse that are meant to be copied (skipped kinds excluded).
    pub source_count: u64,
    /// Of those, how many are here.
    pub target_count: u64,
    /// Rows in Synapse deliberately not copied (hidden devices, rooms joined over federation,
    /// ...), as the copy logged them.
    pub skipped_count: u64,
    /// How many rows were compared field by field.
    pub sampled: u64,
    /// What the samples found different, one line each.
    pub mismatches: Vec<String>,
}

/// A verification of the copy against Synapse: counts, and samples compared field by field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    /// Every count matches and no sample differs.
    pub passed: bool,
    /// When it finished (RFC 3339).
    pub checked_at: String,
    /// Per stream.
    pub streams: Vec<StreamVerification>,
}
