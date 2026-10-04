//! Email pushers: the notification emails a user who registered an `email` pusher receives
//! about messages they have not read.
//!
//! # What happens
//!
//! The pipeline (`crate::pipeline`) hands this module every notification for a user with an
//! email pusher, and every read receipt. A notification does not become an email at once: it
//! is held for the user's address, and the email goes when the hold is due, carrying every
//! room with unread notifications held by then (one email, several rooms), each room's unread
//! count from the notification counts, and the messages themselves as sender, time and
//! snippet (no snippet in an encrypted room). Reading a room in a client takes it out of the
//! held email, so someone at their keyboard is not emailed about what they just read.
//!
//! # When the email goes
//!
//! The first notification in a room waits `delay_before_mail` (zero by default: the first
//! email goes at once). After an email about a room, the next one about it waits
//! `throttle_start`, then `throttle_multiplier` times as long each time, up to
//! `throttle_max`, while the room's messages stay unread; reading the room resets it, and so
//! does `throttle_reset_after` without a notification. These are Synapse's rules and values,
//! except that Synapse also waits ten minutes before the first email. The throttle state is
//! stored ([`throttle`]); what is held for an email is in memory, so a restart loses a pending
//! email (the next notification starts another).
//!
//! # Sending
//!
//! [`Mailer`] is the seam: [`smtp::SmtpMailer`] sends over SMTP with `lettre`; tests record.
//! A failed send is retried twice, a minute apart, then given up with a warning. Metrics:
//! `hs_push_email_sent_total{outcome=sent|failed|skipped}`.

pub mod smtp;
pub mod template;
pub mod throttle;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use ruma::api::client::push::PusherKind;
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::counts::CountsStore;
use crate::pushers::PusherStore;
use template::{LineText, MailInput, NotificationLine, RoomSection};
use throttle::{ThrottleState, ThrottleStore};

/// How many messages per room an email shows; the rest is "and N more".
const LINES_PER_ROOM: usize = 10;
/// How many times a failed send is tried before it is given up.
const SEND_ATTEMPTS: u32 = 3;
/// How long after a failed attempt the next is made.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// The subject of each kind of email, with Synapse's `%(app)s`, `%(person)s` and `%(room)s`
/// placeholders. The same fields as `hs_config::email::SubjectsConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Subjects {
    pub message_from_person_in_room: String,
    pub message_from_person: String,
    pub messages_from_person: String,
    pub messages_in_room: String,
    pub messages_in_room_and_others: String,
    pub messages_from_person_and_others: String,
    pub invite_from_person: String,
    pub invite_from_person_to_room: String,
}

impl Default for Subjects {
    fn default() -> Self {
        Self {
            message_from_person_in_room:
                "[%(app)s] You have a message on %(app)s from %(person)s in the %(room)s room..."
                    .to_owned(),
            message_from_person: "[%(app)s] You have a message on %(app)s from %(person)s..."
                .to_owned(),
            messages_from_person: "[%(app)s] You have messages on %(app)s from %(person)s..."
                .to_owned(),
            messages_in_room: "[%(app)s] You have messages on %(app)s in the %(room)s room..."
                .to_owned(),
            messages_in_room_and_others:
                "[%(app)s] You have messages on %(app)s in the %(room)s room and others..."
                    .to_owned(),
            messages_from_person_and_others:
                "[%(app)s] You have messages on %(app)s from %(person)s and others...".to_owned(),
            invite_from_person: "[%(app)s] %(person)s has invited you to chat on %(app)s..."
                .to_owned(),
            invite_from_person_to_room:
                "[%(app)s] %(person)s has invited you to join the %(room)s room on %(app)s..."
                    .to_owned(),
        }
    }
}

/// What the emails say and when they go. `hs-cli` builds it from `hs_config::email`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Whether notification emails are sent at all.
    pub enabled: bool,
    /// The sender, as `Name <address>` or a bare address.
    pub from: String,
    /// The service's name in subjects and bodies.
    pub app_name: String,
    /// The web client room links open; `None` for `matrix.to`.
    pub client_base_url: Option<String>,
    /// How long the first notification in a room is held.
    pub delay_before_mail: Duration,
    /// The wait after an email about a room before the next one about it.
    pub throttle_start: Duration,
    /// The longest such wait.
    pub throttle_max: Duration,
    /// How much longer each successive wait is.
    pub throttle_multiplier: u32,
    /// A room without a notification for this long starts over at `throttle_start`.
    pub throttle_reset_after: Duration,
    /// The subjects.
    pub subjects: Subjects,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            from: String::new(),
            app_name: "Matrix".to_owned(),
            client_base_url: None,
            delay_before_mail: Duration::ZERO,
            throttle_start: Duration::from_secs(10 * 60),
            throttle_max: Duration::from_secs(24 * 60 * 60),
            throttle_multiplier: 6,
            throttle_reset_after: Duration::from_secs(12 * 60 * 60),
            subjects: Subjects::default(),
        }
    }
}

/// One email to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundMail {
    /// The sender mailbox.
    pub from: String,
    /// The recipient address.
    pub to: String,
    /// The subject.
    pub subject: String,
    /// The plain-text part.
    pub text: String,
    /// The HTML part.
    pub html: String,
}

/// Why an email was not sent.
#[derive(Debug, thiserror::Error)]
pub enum MailError {
    /// No SMTP server is configured.
    #[error("no SMTP server is configured")]
    NotConfigured,
    /// The sender or recipient is not a mailbox.
    #[error("bad address: {0}")]
    Address(String),
    /// The server could not be reached, refused the mail, or the message could not be built.
    #[error("sending failed: {0}")]
    Transport(String),
}

/// Sends email. [`smtp::SmtpMailer`] for a server; tests record.
#[async_trait::async_trait]
pub trait Mailer: Send + Sync {
    /// Whether anything can be sent now.
    fn is_configured(&self) -> bool;

    /// Sends `mail`.
    async fn send(&self, mail: &OutboundMail) -> Result<(), MailError>;
}

/// A notification for a user with an email pusher, as the pipeline hands it over.
#[derive(Debug, Clone)]
pub struct Notification {
    /// The recipient.
    pub user_id: OwnedUserId,
    /// The pusher's address (its pushkey).
    pub address: String,
    /// The room.
    pub room_id: OwnedRoomId,
    /// The room's name, if it has one.
    pub room_name: Option<String>,
    /// The sender's display name, if they have one.
    pub sender_display_name: Option<String>,
    /// The event, in the client-server API's shape.
    pub event: Value,
}

/// What the pipeline tells the worker.
#[derive(Debug, Clone)]
enum Job {
    Notified(Box<Notification>),
    RoomRead {
        user_id: OwnedUserId,
        room_id: OwnedRoomId,
    },
}

#[derive(Debug)]
struct SettingsCell(RwLock<Arc<Settings>>);

impl SettingsCell {
    fn get(&self) -> Arc<Settings> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, settings: Settings) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(settings);
    }
}

/// The sending side: what the pipeline holds, and what a configuration change updates.
#[derive(Debug, Clone)]
pub struct EmailPushersHandle {
    tx: mpsc::UnboundedSender<Job>,
    settings: Arc<SettingsCell>,
}

impl EmailPushersHandle {
    /// A notification fired for a user with an email pusher.
    pub fn notified(&self, notification: Notification) {
        self.send(Job::Notified(Box::new(notification)));
    }

    /// A user read a room: nothing held about it is emailed, and its throttle starts over.
    pub fn room_read(&self, user_id: &UserId, room_id: &RoomId) {
        self.send(Job::RoomRead {
            user_id: user_id.to_owned(),
            room_id: room_id.to_owned(),
        });
    }

    /// Replaces the settings for everything from now on (a configuration change).
    pub fn set_settings(&self, settings: Settings) {
        self.settings.set(settings);
    }

    /// The settings in force.
    #[must_use]
    pub fn settings(&self) -> Arc<Settings> {
        self.settings.get()
    }

    fn send(&self, job: Job) {
        if self.tx.send(job).is_err() {
            tracing::warn!("the email pusher worker has stopped; dropping a job");
        }
    }
}

/// Everything the worker runs on.
pub struct EmailDeps {
    /// To check a pusher is still there when its email is due.
    pub pushers: Arc<dyn PusherStore>,
    /// The unread counts each room's section reports.
    pub counts: Arc<dyn CountsStore>,
    /// Per-room throttle state.
    pub throttle: Arc<dyn ThrottleStore>,
    /// What sends the mail.
    pub mailer: Arc<dyn Mailer>,
}

#[derive(Debug)]
struct PendingRoom {
    name: Option<String>,
    lines: Vec<NotificationLine>,
}

#[derive(Debug)]
struct PendingMail {
    due: Instant,
    attempts: u32,
    rooms: BTreeMap<OwnedRoomId, PendingRoom>,
}

/// The worker: holds notifications and sends the emails when they are due. [`spawn`] runs it;
/// tests drive [`EmailPushersWorker::handle_job`] and [`EmailPushersWorker::send_due`].
pub struct EmailPushersWorker {
    rx: mpsc::UnboundedReceiver<Job>,
    deps: EmailDeps,
    settings: Arc<SettingsCell>,
    pending: HashMap<(OwnedUserId, String), PendingMail>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: &'static str,
}

static SENT: LazyLock<Family<OutcomeLabels, Counter>> = LazyLock::new(Family::default);

/// Registers `hs_push_email_sent_total` (by outcome: `sent`, `failed` after every retry,
/// `skipped` because nothing was left to send or no server is configured).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_push_email_sent",
        "Notification emails to email pushers, by outcome: sent, failed (given up after \
         retries), skipped (no SMTP server, notifications off, the pusher was removed, or \
         everything was read before the email was due)",
        SENT.clone(),
    );
}

fn count(outcome: &'static str) {
    SENT.get_or_create(&OutcomeLabels { outcome }).inc();
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A worker over `deps` with `settings`, and the handle that feeds it. The worker is returned
/// rather than spawned so a caller assembling state outside a runtime can start it later.
#[must_use]
pub fn channel(deps: EmailDeps, settings: Settings) -> (EmailPushersHandle, EmailPushersWorker) {
    let (tx, rx) = mpsc::unbounded_channel();
    let settings = Arc::new(SettingsCell(RwLock::new(Arc::new(settings))));
    let worker = EmailPushersWorker {
        rx,
        deps,
        settings: settings.clone(),
        pending: HashMap::new(),
    };
    (EmailPushersHandle { tx, settings }, worker)
}

/// Starts the worker on a new task.
#[must_use]
pub fn spawn(deps: EmailDeps, settings: Settings) -> EmailPushersHandle {
    let (handle, worker) = channel(deps, settings);
    tokio::spawn(worker.run());
    handle
}

impl EmailPushersWorker {
    /// Handles jobs and sends due emails until the last handle is dropped.
    pub async fn run(mut self) {
        loop {
            let next = self.next_due();
            tokio::select! {
                job = self.rx.recv() => match job {
                    Some(job) => self.handle_job(job).await,
                    None => break,
                },
                () = async {
                    match next {
                        Some(due) => tokio::time::sleep_until(due).await,
                        None => std::future::pending().await,
                    }
                } => self.send_due().await,
            }
        }
        tracing::info!("the email pusher worker stopped: every handle was dropped");
    }

    /// When the earliest held email is due, if any.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.due).min()
    }

    /// How many emails are held.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    async fn handle_job(&mut self, job: Job) {
        match job {
            Job::Notified(notification) => self.hold(*notification).await,
            Job::RoomRead { user_id, room_id } => self.room_read(&user_id, &room_id).await,
        }
    }

    /// Holds `notification` for its address, scheduling the email if none is held yet.
    pub async fn hold(&mut self, notification: Notification) {
        let settings = self.settings.get();
        if !settings.enabled || !self.deps.mailer.is_configured() {
            tracing::debug!(
                user = %notification.user_id,
                enabled = settings.enabled,
                "a notification for an email pusher, but no email can be sent"
            );
            count("skipped");
            return;
        }
        let Some(line) = template::line_for(
            &notification.event,
            notification.sender_display_name.as_deref(),
        ) else {
            return;
        };
        let now = Instant::now();
        let mut due = now + settings.delay_before_mail;
        match self
            .deps
            .throttle
            .get(
                &notification.user_id,
                &notification.room_id,
                &notification.address,
            )
            .await
        {
            Ok(Some(state)) => {
                let since_last = now_ms().saturating_sub(state.last_sent_ms);
                if since_last < throttle_ms(settings.throttle_reset_after) {
                    let wait = state.throttle_ms.saturating_sub(since_last);
                    due = due.max(now + Duration::from_millis(wait));
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(user = %notification.user_id, error = %e, "could not read the email throttle; sending without it");
            }
        }
        let key = (notification.user_id.clone(), notification.address.clone());
        let pending = self.pending.entry(key).or_insert_with(|| PendingMail {
            due,
            attempts: 0,
            rooms: BTreeMap::new(),
        });
        pending.due = pending.due.min(due);
        let room = pending
            .rooms
            .entry(notification.room_id.clone())
            .or_insert_with(|| PendingRoom {
                name: None,
                lines: Vec::new(),
            });
        if notification.room_name.is_some() {
            room.name = notification.room_name;
        }
        room.lines.push(line);
        if room.lines.len() > LINES_PER_ROOM {
            room.lines.remove(0);
        }
        tracing::debug!(
            user = %notification.user_id,
            room = %notification.room_id,
            due_in_ms = pending.due.saturating_duration_since(now).as_millis(),
            "holding a notification for an email"
        );
    }

    /// Takes the room out of everything held for the user and forgets its throttle.
    pub async fn room_read(&mut self, user_id: &UserId, room_id: &RoomId) {
        self.pending.retain(|(user, _), pending| {
            if user == user_id {
                pending.rooms.remove(room_id);
            }
            !pending.rooms.is_empty()
        });
        if let Err(e) = self.deps.throttle.reset_room(user_id, room_id).await {
            tracing::warn!(user = %user_id, room = %room_id, error = %e, "could not reset the email throttle");
        }
    }

    /// Sends every held email whose time has come.
    pub async fn send_due(&mut self) {
        let now = Instant::now();
        let due: Vec<(OwnedUserId, String)> = self
            .pending
            .iter()
            .filter(|(_, p)| p.due <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for key in due {
            let Some(pending) = self.pending.remove(&key) else {
                continue;
            };
            let (user_id, address) = &key;
            match self.send_one(user_id, address, &pending).await {
                Ok(()) => {}
                Err(e) if pending.attempts + 1 < SEND_ATTEMPTS => {
                    tracing::warn!(
                        user = %user_id,
                        attempt = pending.attempts + 1,
                        error = %e,
                        "a notification email could not be sent; trying again"
                    );
                    self.pending.insert(
                        key,
                        PendingMail {
                            due: now + RETRY_AFTER,
                            attempts: pending.attempts + 1,
                            rooms: pending.rooms,
                        },
                    );
                }
                Err(e) => {
                    count("failed");
                    tracing::warn!(
                        user = %user_id,
                        attempts = SEND_ATTEMPTS,
                        error = %e,
                        "a notification email could not be sent; giving up on it"
                    );
                }
            }
        }
    }

    /// Builds and sends one held email. `Ok` when it went, or when there was nothing left to
    /// send; `Err` for a send that should be retried.
    async fn send_one(
        &self,
        user_id: &UserId,
        address: &str,
        pending: &PendingMail,
    ) -> Result<(), MailError> {
        let settings = self.settings.get();
        if !settings.enabled || !self.deps.mailer.is_configured() {
            count("skipped");
            return Ok(());
        }
        let still_there = self
            .deps
            .pushers
            .get_pushers(user_id)
            .await
            .map_err(|e| MailError::Transport(format!("reading pushers: {e}")))?
            .iter()
            .any(|p| {
                matches!(p.kind, PusherKind::Email(_))
                    && p.ids.pushkey.eq_ignore_ascii_case(address)
            });
        if !still_there {
            tracing::debug!(user = %user_id, "the email pusher is gone; not sending its email");
            count("skipped");
            return Ok(());
        }
        let mut rooms = Vec::new();
        for (room_id, room) in &pending.rooms {
            let unread = self
                .deps
                .counts
                .get_room_counts(user_id, room_id)
                .await
                .map_err(|e| MailError::Transport(format!("reading counts: {e}")))?
                .totals()
                .notification_count;
            // Invitations are not counted as unread notifications once answered; a held
            // invitation is shown regardless, since there is no receipt to read it with.
            let is_invite = room.lines.iter().any(|l| l.text == LineText::Invite);
            if unread == 0 && !is_invite {
                continue;
            }
            rooms.push(RoomSection {
                room_id: room_id.clone(),
                name: room.name.clone(),
                unread: unread.max(u64::try_from(room.lines.len()).unwrap_or(u64::MAX)),
                lines: room.lines.clone(),
            });
        }
        if rooms.is_empty() {
            tracing::debug!(user = %user_id, "everything held for an email was read; not sending it");
            count("skipped");
            return Ok(());
        }
        let input = MailInput {
            app_name: &settings.app_name,
            client_base_url: settings.client_base_url.as_deref(),
            subjects: &settings.subjects,
            rooms: &rooms,
        };
        let mail = OutboundMail {
            from: settings.from.clone(),
            to: address.to_owned(),
            subject: template::subject(&input),
            text: template::render_text(&input),
            html: template::render_html(&input),
        };
        self.deps.mailer.send(&mail).await?;
        count("sent");
        tracing::info!(
            user = %user_id,
            rooms = rooms.len(),
            "notification email sent"
        );
        let sent_ms = now_ms();
        for room in &rooms {
            let next_throttle = match self
                .deps
                .throttle
                .get(user_id, &room.room_id, address)
                .await
            {
                Ok(Some(state))
                    if sent_ms.saturating_sub(state.last_sent_ms)
                        < throttle_ms(settings.throttle_reset_after) =>
                {
                    state
                        .throttle_ms
                        .saturating_mul(u64::from(settings.throttle_multiplier.max(1)))
                        .min(throttle_ms(settings.throttle_max))
                }
                Ok(_) => throttle_ms(settings.throttle_start),
                Err(e) => {
                    tracing::warn!(user = %user_id, error = %e, "could not read the email throttle");
                    throttle_ms(settings.throttle_start)
                }
            };
            if let Err(e) = self
                .deps
                .throttle
                .set(
                    user_id,
                    &room.room_id,
                    address,
                    ThrottleState {
                        last_sent_ms: sent_ms,
                        throttle_ms: next_throttle,
                    },
                )
                .await
            {
                tracing::warn!(user = %user_id, error = %e, "could not record the email throttle");
            }
        }
        Ok(())
    }
}

fn throttle_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counts::Scope;
    use crate::counts::memory::InMemoryCountsStore;
    use crate::pushers::memory::InMemoryPusherStore;
    use ruma::api::client::push::{EmailPusherData, Pusher, PusherIds, PusherInit};
    use ruma::{room_id, user_id};
    use serde_json::json;
    use std::sync::Mutex;

    /// Records what it is asked to send; `fail` makes every send fail.
    #[derive(Default)]
    struct RecordingMailer {
        configured: std::sync::atomic::AtomicBool,
        fail: std::sync::atomic::AtomicBool,
        sent: Mutex<Vec<OutboundMail>>,
    }

    impl RecordingMailer {
        fn configured() -> Arc<Self> {
            let m = Self::default();
            m.configured
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Arc::new(m)
        }
        fn sent(&self) -> Vec<OutboundMail> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Mailer for RecordingMailer {
        fn is_configured(&self) -> bool {
            self.configured.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn send(&self, mail: &OutboundMail) -> Result<(), MailError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(MailError::Transport("down".to_owned()));
            }
            self.sent.lock().unwrap().push(mail.clone());
            Ok(())
        }
    }

    struct Harness {
        worker: EmailPushersWorker,
        handle: EmailPushersHandle,
        mailer: Arc<RecordingMailer>,
        counts: Arc<InMemoryCountsStore>,
        pushers: Arc<InMemoryPusherStore>,
    }

    const ADDRESS: &str = "alice@example.org";

    fn email_pusher(address: &str) -> Pusher {
        PusherInit {
            ids: PusherIds::new(address.to_owned(), "m.email".to_owned()),
            kind: PusherKind::Email(EmailPusherData::new()),
            app_display_name: "Email Notifications".to_owned(),
            device_display_name: address.to_owned(),
            profile_tag: None,
            lang: "en".to_owned(),
        }
        .into()
    }

    async fn harness(settings: Settings) -> Harness {
        let mailer = RecordingMailer::configured();
        let counts = Arc::new(InMemoryCountsStore::new());
        let pushers = Arc::new(InMemoryPusherStore::new());
        pushers
            .set_pusher(user_id!("@alice:example.org"), email_pusher(ADDRESS), None)
            .await
            .unwrap();
        let (handle, worker) = channel(
            EmailDeps {
                pushers: pushers.clone(),
                counts: counts.clone(),
                throttle: Arc::new(throttle::InMemoryThrottleStore::new()),
                mailer: mailer.clone(),
            },
            settings,
        );
        Harness {
            worker,
            handle,
            mailer,
            counts,
            pushers,
        }
    }

    fn settings() -> Settings {
        Settings {
            from: "Myelin <noreply@example.org>".to_owned(),
            app_name: "Myelin".to_owned(),
            client_base_url: Some("https://app.example.org".to_owned()),
            ..Settings::default()
        }
    }

    fn message(room: &RoomId, body: &str) -> Notification {
        Notification {
            user_id: user_id!("@alice:example.org").to_owned(),
            address: ADDRESS.to_owned(),
            room_id: room.to_owned(),
            room_name: Some("Lunch".to_owned()),
            sender_display_name: Some("Bob".to_owned()),
            event: json!({
                "event_id": format!("${body}:example.org"),
                "room_id": room,
                "type": "m.room.message",
                "sender": "@bob:example.org",
                "origin_server_ts": 1_700_000_000_000u64,
                "content": {"msgtype": "m.text", "body": body},
            }),
        }
    }

    /// A notification the pipeline would have counted: counts first, then the hold.
    async fn notify(h: &mut Harness, room: &RoomId, body: &str) {
        h.counts
            .record_notification(user_id!("@alice:example.org"), room, Scope::Main, false)
            .await
            .unwrap();
        h.worker.hold(message(room, body)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_email_goes_at_once_and_the_next_waits_longer_each_time() {
        let mut h = harness(settings()).await;
        let room = room_id!("!room:example.org");
        notify(&mut h, room, "hello").await;
        assert_eq!(h.worker.next_due(), Some(Instant::now()));
        h.worker.send_due().await;
        let sent = h.mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, ADDRESS);
        assert_eq!(sent[0].from, "Myelin <noreply@example.org>");
        assert_eq!(
            sent[0].subject,
            "[Myelin] You have a message on Myelin from Bob in the Lunch room..."
        );
        assert!(sent[0].text.contains("Lunch (1 unread)"));
        assert!(sent[0].text.contains("Bob at 22:13 UTC: hello"));
        assert!(
            sent[0]
                .html
                .contains("https://app.example.org/#/room/!room:example.org")
        );
        assert_eq!(h.worker.pending_count(), 0);

        // The second notification in the room waits `throttle_start` (10 min); a third one
        // meanwhile joins the same email.
        notify(&mut h, room, "again").await;
        let due = h.worker.next_due().unwrap();
        let wait = due.saturating_duration_since(Instant::now());
        assert!(
            wait > Duration::from_secs(9 * 60) && wait <= Duration::from_secs(10 * 60),
            "{wait:?}"
        );
        notify(&mut h, room, "and again").await;
        assert_eq!(h.worker.pending_count(), 1);
        h.worker.send_due().await;
        assert_eq!(h.mailer.sent().len(), 1, "not due yet");
        tokio::time::advance(wait).await;
        h.worker.send_due().await;
        let sent = h.mailer.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[1].subject,
            "[Myelin] You have messages on Myelin in the Lunch room..."
        );
        assert!(sent[1].text.contains("Lunch (3 unread)"));
        assert!(sent[1].text.contains("again"));
        assert!(sent[1].text.contains("and again"));
        assert!(!sent[1].text.contains("hello"), "already mailed");

        // The third waits six times as long.
        notify(&mut h, room, "once more").await;
        let wait = h
            .worker
            .next_due()
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert!(
            wait > Duration::from_secs(59 * 60) && wait <= Duration::from_secs(60 * 60),
            "{wait:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reading_the_room_cancels_the_email_and_resets_the_wait() {
        let mut h = harness(Settings {
            delay_before_mail: Duration::from_secs(600),
            ..settings()
        })
        .await;
        let room = room_id!("!room:example.org");
        let alice = user_id!("@alice:example.org");
        notify(&mut h, room, "hello").await;
        assert_eq!(h.worker.pending_count(), 1);
        h.counts.reset_room(alice, room).await.unwrap();
        h.worker.room_read(alice, room).await;
        assert_eq!(h.worker.pending_count(), 0);
        tokio::time::advance(Duration::from_secs(601)).await;
        h.worker.send_due().await;
        assert!(h.mailer.sent().is_empty());

        // Mail once, read, and the next notification is again only the first-mail delay away.
        notify(&mut h, room, "x").await;
        tokio::time::advance(Duration::from_secs(600)).await;
        h.worker.send_due().await;
        assert_eq!(h.mailer.sent().len(), 1);
        h.counts.reset_room(alice, room).await.unwrap();
        h.worker.room_read(alice, room).await;
        notify(&mut h, room, "y").await;
        let wait = h
            .worker
            .next_due()
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert_eq!(wait, Duration::from_secs(600));
    }

    #[tokio::test(start_paused = true)]
    async fn one_email_covers_several_rooms_and_a_read_room_drops_out_of_it() {
        let mut h = harness(Settings {
            delay_before_mail: Duration::from_secs(60),
            ..settings()
        })
        .await;
        let lunch = room_id!("!lunch:example.org");
        let other = room_id!("!other:example.org");
        notify(&mut h, lunch, "soup?").await;
        let mut dm = message(other, "psst");
        dm.room_name = None;
        dm.event["type"] = json!("m.room.encrypted");
        h.counts
            .record_notification(user_id!("@alice:example.org"), other, Scope::Main, false)
            .await
            .unwrap();
        h.worker.hold(dm).await;
        assert_eq!(h.worker.pending_count(), 1, "one email for both rooms");
        // Lunch is read before the email goes; only the direct chat is left.
        h.counts
            .reset_room(user_id!("@alice:example.org"), lunch)
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(60)).await;
        h.worker.send_due().await;
        let sent = h.mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].subject,
            "[Myelin] You have a message on Myelin from Bob..."
        );
        assert!(!sent[0].text.contains("Lunch"));
        assert!(sent[0].text.contains("Bob (1 unread)"));
        assert!(sent[0].text.contains("an encrypted message"));
        assert!(!sent[0].text.contains("psst"), "ciphertext is never quoted");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_send_is_retried_and_then_given_up() {
        let mut h = harness(settings()).await;
        let room = room_id!("!room:example.org");
        h.mailer
            .fail
            .store(true, std::sync::atomic::Ordering::SeqCst);
        notify(&mut h, room, "hello").await;
        h.worker.send_due().await;
        assert_eq!(h.worker.pending_count(), 1, "held for a retry");
        tokio::time::advance(RETRY_AFTER).await;
        h.worker.send_due().await;
        assert_eq!(h.worker.pending_count(), 1);
        tokio::time::advance(RETRY_AFTER).await;
        h.worker.send_due().await;
        assert_eq!(h.worker.pending_count(), 0, "given up");
        assert!(h.mailer.sent().is_empty());

        // The server comes back: the next notification is mailed on the first try.
        h.mailer
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        notify(&mut h, room, "hello?").await;
        h.worker.send_due().await;
        assert_eq!(h.mailer.sent().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_without_a_server_or_when_the_pusher_is_gone() {
        let mut h = harness(settings()).await;
        let room = room_id!("!room:example.org");
        h.mailer
            .configured
            .store(false, std::sync::atomic::Ordering::SeqCst);
        notify(&mut h, room, "hello").await;
        assert_eq!(h.worker.pending_count(), 0, "not even held");

        h.mailer
            .configured
            .store(true, std::sync::atomic::Ordering::SeqCst);
        h.handle.set_settings(Settings {
            enabled: false,
            ..settings()
        });
        notify(&mut h, room, "hello").await;
        assert_eq!(h.worker.pending_count(), 0);

        h.handle.set_settings(settings());
        notify(&mut h, room, "hello").await;
        assert_eq!(h.worker.pending_count(), 1);
        h.pushers
            .delete_pusher(
                user_id!("@alice:example.org"),
                &PusherIds::new(ADDRESS.to_owned(), "m.email".to_owned()),
            )
            .await
            .unwrap();
        h.worker.send_due().await;
        assert!(h.mailer.sent().is_empty());
    }

    #[tokio::test]
    async fn the_spawned_worker_sends_from_the_handle() {
        let mailer = RecordingMailer::configured();
        let counts = Arc::new(InMemoryCountsStore::new());
        let pushers = Arc::new(InMemoryPusherStore::new());
        let alice = user_id!("@alice:example.org");
        let room = room_id!("!room:example.org");
        pushers
            .set_pusher(alice, email_pusher(ADDRESS), None)
            .await
            .unwrap();
        counts
            .record_notification(alice, room, Scope::Main, false)
            .await
            .unwrap();
        let handle = spawn(
            EmailDeps {
                pushers,
                counts,
                throttle: Arc::new(throttle::InMemoryThrottleStore::new()),
                mailer: mailer.clone(),
            },
            settings(),
        );
        handle.notified(message(room, "hello"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while mailer.sent().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(mailer.sent().len(), 1);
    }
}
