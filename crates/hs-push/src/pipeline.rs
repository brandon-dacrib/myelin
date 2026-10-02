//! The push pipeline: every accepted room event is evaluated against each local member's rules;
//! a match that notifies counts toward the room's unread numbers, lands in the user's
//! notification log, and goes out to each of their HTTP pushers. A read receipt zeroes the
//! room's counts, marks the log read, and sends each pusher the new badge.
//!
//! # Where the events come from
//!
//! This crate does not depend on `hs-room`. The room layer hands the pipeline event references
//! (`room_id`, `event_id`, the room-local `room_pos`) through [`PipelineHandle`], and the
//! pipeline asks an [`EventSource`] for everything it needs to know about one: the event's
//! client-form JSON, the room's members and power levels, its name. `hs-cli` implements the
//! source over the room registry (`crates/hs-cli/src/push_delivery.rs`) and forwards the room
//! stream, the same shape `hs-appservice`'s transaction pump uses.
//!
//! # Once per event
//!
//! The room stream re-announces a room's newest event whenever the room is loaded, so after a
//! restart every room's last event comes by again. [`crate::cursors`] remembers the last
//! `room_pos` evaluated per room; an event at or below it is skipped. A room the pipeline has
//! never seen whose announced event is older than the pipeline itself is taken as such a
//! re-announcement too (the alternative, pushing every room's last message once after an
//! upgrade, would be a visible bug on day one), with [`STALE_HEAD_GRACE`] of slack for clocks.
//!
//! # Delivery
//!
//! Evaluation, counting and logging happen in order on one task, so a user's counts are exact
//! as of each event. HTTP delivery is spawned per push: a slow or down gateway delays nothing
//! but its own pushes. A gateway that answers with `rejected` pushkeys gets those pushers
//! deleted, per the Push Gateway API.

use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use ruma::api::client::push::{Pusher, PusherKind};
use ruma::push::{Action, PushFormat};
use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedServerName, OwnedUserId, RoomId, UserId};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::context::{PushEvaluationInput, PushEvaluationMember, build_room_ctx};
use crate::counts::{CountsStore, Scope};
use crate::cursors::CursorStore;
use crate::engine::{EvaluationOutcome, evaluate, flatten_event};
use crate::error::StoreError;
use crate::notification_log::{NewNotification, NotificationLogStore};
use crate::pushers::PusherStore;
use crate::pushers::http::HttpPusherClient;
use crate::ruleset::Ruleset;
use crate::rulesets::{CachedRulesetStore, RulesetStore};

/// How much older than the pipeline's start an event may be, in a room the pipeline has never
/// seen, before it is taken for a re-announced head rather than news. See the module docs.
pub const STALE_HEAD_GRACE_MS: u64 = 60_000;

/// Everything the pipeline needs to know about one event, from the room layer.
#[derive(Debug, Clone)]
pub struct DescribedEvent {
    /// The event in the client-server API's shape (`event_id`, `type`, `sender`, `content`,
    /// `state_key`, `origin_server_ts`, ...).
    pub event: Value,
    /// The room's members, member count and power levels, as evaluation needs them.
    pub input: PushEvaluationInput,
    /// The room's human-readable name: `m.room.name`, else its canonical alias, else what a
    /// client would show (the other members' names), else nothing.
    pub room_name: Option<String>,
    /// The sender's display name in the room, if they have one.
    pub sender_display_name: Option<String>,
}

/// The room layer's side of the pipeline: answers "tell me about this event".
#[async_trait::async_trait]
pub trait EventSource: Send + Sync {
    /// Describes `event_id` in `room_id`, or `None` if this process should not push for it: the
    /// room is not this replica's, the event is unknown here, or it was rejected.
    async fn describe(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<DescribedEvent>, String>;
}

/// A user's effective ruleset, as the pipeline reads it: the cached store behind a trait object,
/// so the pipeline is not generic over the storage backend.
#[async_trait::async_trait]
pub trait EffectiveRulesets: Send + Sync {
    /// The ruleset to evaluate `user_id` against.
    async fn effective_ruleset(&self, user_id: &UserId) -> Result<Arc<Ruleset>, StoreError>;
}

#[async_trait::async_trait]
impl<S: RulesetStore> EffectiveRulesets for CachedRulesetStore<S> {
    async fn effective_ruleset(&self, user_id: &UserId) -> Result<Arc<Ruleset>, StoreError> {
        CachedRulesetStore::effective_ruleset(self, user_id).await
    }
}

/// What the room and session layers tell the pipeline.
#[derive(Debug, Clone)]
pub enum Job {
    /// A room persisted an event.
    Event {
        /// The room.
        room_id: OwnedRoomId,
        /// The event.
        event_id: OwnedEventId,
        /// The event's room-local stream position, for [`crate::cursors`].
        room_pos: i64,
    },
    /// A local user sent a read receipt for a room.
    Receipt {
        /// The reader.
        user_id: OwnedUserId,
        /// The room.
        room_id: OwnedRoomId,
    },
}

/// The sending side of the pipeline, for the room stream forwarder and the receipt hook.
#[derive(Debug, Clone)]
pub struct PipelineHandle {
    tx: mpsc::UnboundedSender<Job>,
}

impl PipelineHandle {
    /// Tells the pipeline a room persisted an event.
    pub fn event_persisted(&self, room_id: OwnedRoomId, event_id: OwnedEventId, room_pos: i64) {
        self.send(Job::Event {
            room_id,
            event_id,
            room_pos,
        });
    }

    /// Tells the pipeline a user read a room (any `m.read` or `m.read.private` receipt).
    pub fn receipt(&self, user_id: OwnedUserId, room_id: OwnedRoomId) {
        self.send(Job::Receipt { user_id, room_id });
    }

    fn send(&self, job: Job) {
        if self.tx.send(job).is_err() {
            tracing::warn!("the push pipeline has stopped; dropping a job");
        }
    }
}

/// What a session layer calls when a local user's read receipt lands; implemented by
/// [`PipelineHandle`]. A trait so `hs-user` can hold one without naming the pipeline.
pub trait ReadReceiptSink: Send + Sync {
    /// `user_id` read `room_id` (up to some event; the pipeline treats the whole room as read).
    fn read_receipt(&self, user_id: &UserId, room_id: &RoomId);
}

impl ReadReceiptSink for PipelineHandle {
    fn read_receipt(&self, user_id: &UserId, room_id: &RoomId) {
        self.receipt(user_id.to_owned(), room_id.to_owned());
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct OutcomeLabels {
    outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ResultLabels {
    result: &'static str,
}

static EVALUATIONS: LazyLock<Family<ResultLabels, Counter>> = LazyLock::new(Family::default);
static HTTP_PUSHES: LazyLock<Family<OutcomeLabels, Counter>> = LazyLock::new(Family::default);

/// Registers this module's metrics: `hs_push_evaluations_total` (by result: `notify`, `silent`,
/// `none`) and `hs_push_http_pushes_total` (by outcome: `sent`, `rejected`, `failed`).
pub fn register_metrics(registry: &mut prometheus_client::registry::Registry) {
    registry.register(
        "hs_push_evaluations",
        "Push rule evaluations of one event for one local member, by result: notify (a rule \
         with a notify action matched), silent (a rule matched without notifying), none (no \
         rule matched)",
        EVALUATIONS.clone(),
    );
    registry.register(
        "hs_push_http_pushes",
        "Notifications posted to HTTP push gateways, by outcome: sent, rejected (the gateway \
         rejected the pushkey and the pusher was removed), failed (no answer after retries)",
        HTTP_PUSHES.clone(),
    );
}

fn count_evaluation(result: &'static str) {
    EVALUATIONS.get_or_create(&ResultLabels { result }).inc();
}

fn count_push(outcome: &'static str) {
    HTTP_PUSHES.get_or_create(&OutcomeLabels { outcome }).inc();
}

/// Everything the pipeline runs on.
pub struct PipelineDeps {
    /// This server's name: receipts from other servers' users are not this pipeline's.
    pub server_name: OwnedServerName,
    /// The room layer.
    pub source: Arc<dyn EventSource>,
    /// Per-user rules.
    pub rulesets: Arc<dyn EffectiveRulesets>,
    /// Pushers to deliver to (and to delete when a gateway rejects them).
    pub pushers: Arc<dyn PusherStore>,
    /// Unread counts.
    pub counts: Arc<dyn CountsStore>,
    /// The `/notifications` log.
    pub log: Arc<dyn NotificationLogStore>,
    /// Per-room progress.
    pub cursors: Arc<dyn CursorStore>,
    /// The gateway client.
    pub http: Arc<HttpPusherClient>,
}

/// The pipeline's worker. [`spawn`] runs it on its own task; tests drive
/// [`Pipeline::handle`] directly.
pub struct Pipeline {
    deps: PipelineDeps,
    started_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The receiving half of a [`channel`]: runs the pipeline until every [`PipelineHandle`] is
/// dropped.
pub struct PipelineWorker {
    pipeline: Pipeline,
    rx: mpsc::UnboundedReceiver<Job>,
}

impl PipelineWorker {
    /// Handles jobs until the last handle is dropped.
    pub async fn run(mut self) {
        while let Some(job) = self.rx.recv().await {
            self.pipeline.handle(job).await;
        }
        tracing::info!("the push pipeline stopped: every handle was dropped");
    }
}

/// A pipeline and the handle that feeds it. The worker is returned rather than spawned so a
/// caller assembling state outside a runtime (`hs-cli`'s mounts) can start it later.
#[must_use]
pub fn channel(deps: PipelineDeps) -> (PipelineHandle, PipelineWorker) {
    let (tx, rx) = mpsc::unbounded_channel();
    let worker = PipelineWorker {
        pipeline: Pipeline::new(deps),
        rx,
    };
    (PipelineHandle { tx }, worker)
}

/// Starts the pipeline on a new task and returns the handle the room and session layers feed.
#[must_use]
pub fn spawn(deps: PipelineDeps) -> PipelineHandle {
    let (handle, worker) = channel(deps);
    tokio::spawn(worker.run());
    handle
}

impl Pipeline {
    /// A pipeline over `deps`, started now.
    #[must_use]
    pub fn new(deps: PipelineDeps) -> Self {
        Self {
            deps,
            started_ms: now_ms(),
        }
    }

    /// Runs one job to completion (its HTTP deliveries excepted, which are spawned). Errors are
    /// logged, not returned: the stream must keep moving.
    pub async fn handle(&self, job: Job) {
        match job {
            Job::Event {
                room_id,
                event_id,
                room_pos,
            } => {
                if let Err(e) = self.handle_event(&room_id, &event_id, room_pos).await {
                    tracing::warn!(room = %room_id, event = %event_id, error = %e, "push evaluation failed");
                }
            }
            Job::Receipt { user_id, room_id } => {
                if let Err(e) = self.handle_receipt(&user_id, &room_id).await {
                    tracing::warn!(room = %room_id, user = %user_id, error = %e, "push receipt handling failed");
                }
            }
        }
    }

    /// Evaluates one event for every local member it concerns. Returns how many members were
    /// notified, or `None` if the event was skipped (already evaluated, or not described).
    pub async fn handle_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        room_pos: i64,
    ) -> Result<Option<usize>, String> {
        let cursor = self
            .deps
            .cursors
            .get(room_id)
            .await
            .map_err(|e| e.to_string())?;
        if cursor.is_some_and(|c| room_pos <= c) {
            tracing::trace!(room = %room_id, event = %event_id, room_pos, "already evaluated");
            return Ok(None);
        }
        let advance = || async {
            self.deps
                .cursors
                .set(room_id, room_pos)
                .await
                .map_err(|e| e.to_string())
        };
        let Some(described) = self.deps.source.describe(room_id, event_id).await? else {
            advance().await?;
            return Ok(None);
        };
        let origin_ts = described.event["origin_server_ts"].as_u64().unwrap_or(0);
        if cursor.is_none() && origin_ts + STALE_HEAD_GRACE_MS < self.started_ms {
            tracing::debug!(
                room = %room_id,
                event = %event_id,
                "first sight of a room whose newest event predates this pipeline; not pushing it"
            );
            advance().await?;
            return Ok(None);
        }

        let notified = self.evaluate_for_members(room_id, &described).await?;
        advance().await?;
        Ok(Some(notified))
    }

    async fn evaluate_for_members(
        &self,
        room_id: &RoomId,
        described: &DescribedEvent,
    ) -> Result<usize, String> {
        let event = &described.event;
        let flattened = flatten_event(&event.to_string()).map_err(|e| e.to_string())?;
        let sender = event["sender"].as_str().unwrap_or_default();
        let is_invite =
            event["type"] == "m.room.member" && event["content"]["membership"] == "invite";
        let invitee = is_invite.then(|| event["state_key"].as_str()).flatten();
        let mut notified = 0;
        for member in &described.input.members {
            if !member.is_local || member.user_id == sender {
                continue;
            }
            let concerned = member.membership == "join"
                || (member.membership == "invite" && invitee == Some(member.user_id.as_str()));
            if !concerned {
                continue;
            }
            let ruleset = self
                .deps
                .rulesets
                .effective_ruleset(&member.user_id)
                .await
                .map_err(|e| e.to_string())?;
            let ctx = build_room_ctx(room_id, &described.input, member);
            let outcome = evaluate(&ruleset, &flattened, &ctx).await;
            match &outcome {
                Some(o) if o.notify => count_evaluation("notify"),
                Some(_) => count_evaluation("silent"),
                None => count_evaluation("none"),
            }
            let Some(outcome) = outcome.filter(|o| o.notify) else {
                continue;
            };
            tracing::debug!(
                room = %room_id,
                event = %event["event_id"],
                user = %member.user_id,
                rule = %outcome.rule_id,
                highlight = outcome.highlight,
                "push rule matched"
            );
            notified += 1;
            self.notify_member(room_id, described, member, &outcome)
                .await?;
        }
        Ok(notified)
    }

    async fn notify_member(
        &self,
        room_id: &RoomId,
        described: &DescribedEvent,
        member: &PushEvaluationMember,
        outcome: &EvaluationOutcome,
    ) -> Result<(), String> {
        let event = &described.event;
        let event_id: OwnedEventId = event["event_id"]
            .as_str()
            .and_then(|s| EventId::parse(s).ok())
            .ok_or_else(|| "described event has no event_id".to_owned())?;
        let thread_root = thread_root(event);
        let scope = match &thread_root {
            Some(root) => Scope::Thread(root),
            None => Scope::Main,
        };
        self.deps
            .counts
            .record_notification(&member.user_id, room_id, scope, outcome.highlight)
            .await
            .map_err(|e| e.to_string())?;
        self.deps
            .log
            .append(
                &member.user_id,
                NewNotification {
                    room_id: room_id.to_owned(),
                    event_id,
                    event: event.clone(),
                    actions: outcome.actions.clone(),
                    profile_tag: None,
                    ts_ms: now_ms(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;

        let pushers = self
            .deps
            .pushers
            .get_pushers(&member.user_id)
            .await
            .map_err(|e| e.to_string())?;
        if pushers.is_empty() {
            return Ok(());
        }
        let unread = self
            .deps
            .counts
            .total_unread(&member.user_id)
            .await
            .map_err(|e| e.to_string())?;
        for pusher in pushers {
            let PusherKind::Http(data) = &pusher.kind else {
                continue;
            };
            let payload = event_notification(
                described,
                &member.user_id,
                outcome,
                unread,
                &pusher,
                data.format.as_ref() == Some(&PushFormat::EventIdOnly),
            );
            self.spawn_delivery(
                member.user_id.clone(),
                pusher.clone(),
                data.url.clone(),
                payload,
            );
        }
        Ok(())
    }

    /// Zeroes the room's counts, marks its log entries read, and tells each pusher the new
    /// badge. Receipts from other servers' users are not this pipeline's.
    pub async fn handle_receipt(&self, user_id: &UserId, room_id: &RoomId) -> Result<(), String> {
        if user_id.server_name() != self.deps.server_name {
            return Ok(());
        }
        self.deps
            .counts
            .reset_room(user_id, room_id)
            .await
            .map_err(|e| e.to_string())?;
        self.deps
            .log
            .mark_room_read(user_id, room_id)
            .await
            .map_err(|e| e.to_string())?;
        let pushers = self
            .deps
            .pushers
            .get_pushers(user_id)
            .await
            .map_err(|e| e.to_string())?;
        if pushers.is_empty() {
            return Ok(());
        }
        let unread = self
            .deps
            .counts
            .total_unread(user_id)
            .await
            .map_err(|e| e.to_string())?;
        for pusher in pushers {
            let PusherKind::Http(data) = &pusher.kind else {
                continue;
            };
            let payload = badge_notification(unread, &pusher);
            self.spawn_delivery(
                user_id.to_owned(),
                pusher.clone(),
                data.url.clone(),
                payload,
            );
        }
        Ok(())
    }

    fn spawn_delivery(&self, user_id: OwnedUserId, pusher: Pusher, url: String, payload: Value) {
        let http = self.deps.http.clone();
        let pushers = self.deps.pushers.clone();
        tokio::spawn(async move {
            deliver(&http, pushers.as_ref(), &user_id, &pusher, &url, &payload).await;
        });
    }
}

/// Posts one notification and acts on the answer: a rejected pushkey is deleted.
async fn deliver(
    http: &HttpPusherClient,
    pushers: &dyn PusherStore,
    user_id: &UserId,
    pusher: &Pusher,
    url: &str,
    payload: &Value,
) {
    match http.notify(url, payload).await {
        Ok(rejected) => {
            if rejected.contains(&pusher.ids.pushkey) {
                count_push("rejected");
                tracing::info!(
                    user = %user_id,
                    app_id = %pusher.ids.app_id,
                    pushkey = %pusher.ids.pushkey,
                    "the push gateway rejected this pushkey; removing the pusher"
                );
                if let Err(e) = pushers.delete_pusher(user_id, &pusher.ids).await {
                    tracing::warn!(user = %user_id, error = %e, "could not remove a rejected pusher");
                }
            } else {
                count_push("sent");
                tracing::debug!(user = %user_id, app_id = %pusher.ids.app_id, url, "push sent");
            }
        }
        Err(e) => {
            count_push("failed");
            tracing::warn!(user = %user_id, app_id = %pusher.ids.app_id, url, error = %e, "push failed");
        }
    }
}

/// The root of the thread `event` is in, if it carries an `m.thread` relation.
fn thread_root(event: &Value) -> Option<OwnedEventId> {
    let relation = event.get("content")?.get("m.relates_to")?;
    if relation.get("rel_type")?.as_str()? != "m.thread" {
        return None;
    }
    EventId::parse(relation.get("event_id")?.as_str()?).ok()
}

/// The pusher's `data` as the gateway sees it: everything given at registration but `url`.
fn device_data(pusher: &Pusher) -> Value {
    let PusherKind::Http(data) = &pusher.kind else {
        return Value::Object(Map::new());
    };
    let mut out: Map<String, Value> = data.data.clone().into_iter().collect();
    if let Some(format) = &data.format {
        out.insert("format".to_owned(), json!(format.as_str()));
    }
    Value::Object(out)
}

fn device(pusher: &Pusher, tweaks: Option<Value>) -> Value {
    let mut device = json!({
        "app_id": pusher.ids.app_id,
        "pushkey": pusher.ids.pushkey,
        "pushkey_ts": 0,
        "data": device_data(pusher),
    });
    if let Some(tweaks) = tweaks {
        device["tweaks"] = tweaks;
    }
    device
}

/// The `tweaks` of a matched rule's actions, as the gateway wants them: `{"highlight": true,
/// "sound": "default"}` and so on. Read off each action's wire form (`{"set_tweak": name,
/// "value": v}`), so a tweak this crate's Ruma does not know by name passes through unchanged;
/// `highlight` without a value means `true`, as the spec says.
fn tweaks_of(actions: &[Action]) -> Value {
    let mut tweaks = Map::new();
    for action in actions {
        let Ok(Value::Object(object)) = serde_json::to_value(action) else {
            continue;
        };
        let Some(Value::String(name)) = object.get("set_tweak") else {
            continue;
        };
        let value = match object.get("value") {
            Some(v) => v.clone(),
            None if name == "highlight" => json!(true),
            None => continue,
        };
        tweaks.insert(name.clone(), value);
    }
    Value::Object(tweaks)
}

/// The Push Gateway API body for an event, for one pusher.
fn event_notification(
    described: &DescribedEvent,
    user_id: &UserId,
    outcome: &EvaluationOutcome,
    unread: u64,
    pusher: &Pusher,
    event_id_only: bool,
) -> Value {
    let event = &described.event;
    let prio =
        if event["type"] == "m.room.encrypted" || outcome.highlight || outcome.sound.is_some() {
            "high"
        } else {
            "low"
        };
    if event_id_only {
        return json!({
            "notification": {
                "event_id": event["event_id"],
                "room_id": event["room_id"],
                "prio": prio,
                "counts": {"unread": unread},
                "devices": [device(pusher, Some(json!({})))],
            }
        });
    }
    let mut notification = json!({
        "id": event["event_id"],
        "event_id": event["event_id"],
        "room_id": event["room_id"],
        "type": event["type"],
        "sender": event["sender"],
        "prio": prio,
        "content": event["content"],
        "counts": {"unread": unread},
        "devices": [device(pusher, Some(tweaks_of(&outcome.actions)))],
    });
    if event["type"] == "m.room.member" && event.get("state_key").is_some() {
        notification["membership"] = event["content"]["membership"].clone();
        notification["user_is_target"] = json!(event["state_key"] == user_id.as_str());
    }
    if let Some(name) = &described.sender_display_name {
        notification["sender_display_name"] = json!(name);
    }
    if let Some(name) = &described.room_name {
        notification["room_name"] = json!(name);
    }
    json!({ "notification": notification })
}

/// The Push Gateway API body that only updates the badge, sent when a receipt changes the
/// unread count.
fn badge_notification(unread: u64, pusher: &Pusher) -> Value {
    json!({
        "notification": {
            "id": "",
            "type": null,
            "sender": "",
            "counts": {"unread": unread},
            "devices": [device(pusher, None)],
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counts::memory::InMemoryCountsStore;
    use crate::cursors::InMemoryCursorStore;
    use crate::notification_log::memory::InMemoryNotificationLogStore;
    use crate::pushers::http::RetryPolicy;
    use crate::pushers::memory::InMemoryPusherStore;
    use crate::rulesets::memory::InMemoryRulesetStore;
    use ruma::api::client::push::{PusherIds, PusherInit};
    use ruma::push::HttpPusherData;
    use ruma::{room_id, user_id};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A fixed room: Alice and Bob joined (both local), Carol invited, Dave remote.
    struct FakeSource {
        events: Mutex<HashMap<OwnedEventId, DescribedEvent>>,
    }

    fn member(id: &str, membership: &str, local: bool) -> PushEvaluationMember {
        PushEvaluationMember {
            user_id: UserId::parse(id).unwrap(),
            membership: membership.to_owned(),
            display_name: id.to_owned(),
            is_local: local,
        }
    }

    fn describe(event: Value) -> DescribedEvent {
        DescribedEvent {
            event,
            input: PushEvaluationInput {
                joined_member_count: 3,
                members: vec![
                    member("@alice:example.org", "join", true),
                    member("@bob:example.org", "join", true),
                    member("@carol:example.org", "invite", true),
                    member("@dave:remote.org", "join", false),
                ],
                power_levels: None,
                room_version: ruma::RoomVersionId::V11,
            },
            room_name: Some("Test Name".to_owned()),
            sender_display_name: Some("Bob".to_owned()),
        }
    }

    #[async_trait::async_trait]
    impl EventSource for FakeSource {
        async fn describe(
            &self,
            _room_id: &RoomId,
            event_id: &EventId,
        ) -> Result<Option<DescribedEvent>, String> {
            Ok(self.events.lock().unwrap().get(event_id).cloned())
        }
    }

    struct Harness {
        pipeline: Pipeline,
        source: Arc<FakeSource>,
        counts: Arc<InMemoryCountsStore>,
        log: Arc<InMemoryNotificationLogStore>,
        pushers: Arc<InMemoryPusherStore>,
    }

    fn harness() -> Harness {
        let source = Arc::new(FakeSource {
            events: Mutex::new(HashMap::new()),
        });
        let counts = Arc::new(InMemoryCountsStore::new());
        let log = Arc::new(InMemoryNotificationLogStore::new());
        let pushers = Arc::new(InMemoryPusherStore::new());
        let deps = PipelineDeps {
            server_name: ruma::server_name!("example.org").to_owned(),
            source: source.clone(),
            rulesets: Arc::new(CachedRulesetStore::new(InMemoryRulesetStore::new())),
            pushers: pushers.clone(),
            counts: counts.clone(),
            log: log.clone(),
            cursors: Arc::new(InMemoryCursorStore::new()),
            http: Arc::new(HttpPusherClient::new(RetryPolicy {
                max_attempts: 1,
                base_backoff: std::time::Duration::from_millis(1),
                max_backoff: std::time::Duration::from_millis(1),
            })),
        };
        Harness {
            pipeline: Pipeline::new(deps),
            source,
            counts,
            log,
            pushers,
        }
    }

    fn message(id: &str, body: &str) -> Value {
        json!({
            "event_id": id,
            "room_id": "!room:example.org",
            "type": "m.room.message",
            "sender": "@bob:example.org",
            "origin_server_ts": now_ms(),
            "content": {"msgtype": "m.text", "body": body},
        })
    }

    fn http_pusher(pushkey: &str, url: &str) -> Pusher {
        PusherInit {
            ids: PusherIds::new(pushkey.to_owned(), "sytest".to_owned()),
            kind: PusherKind::Http(HttpPusherData::new(url.to_owned())),
            app_display_name: "SyTest".to_owned(),
            device_display_name: "a device".to_owned(),
            profile_tag: Some("tag".to_owned()),
            lang: "en".to_owned(),
        }
        .into()
    }

    #[tokio::test]
    async fn a_message_notifies_every_other_joined_local_member_once() {
        let h = harness();
        let room = room_id!("!room:example.org");
        let ev = ruma::event_id!("$1:example.org");
        h.source
            .events
            .lock()
            .unwrap()
            .insert(ev.to_owned(), describe(message(ev.as_str(), "hello")));

        let notified = h.pipeline.handle_event(room, ev, 1).await.unwrap();
        assert_eq!(
            notified,
            Some(1),
            "alice only: bob sent it, carol is invited, dave is remote"
        );
        let alice = user_id!("@alice:example.org");
        let counts = h.counts.get_room_counts(alice, room).await.unwrap();
        assert_eq!(counts.main.notification_count, 1);
        assert_eq!(
            h.counts
                .total_unread(user_id!("@bob:example.org"))
                .await
                .unwrap(),
            0
        );
        let log = h.log.page(alice, None, 10, false).await.unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].event["content"]["body"], "hello");
        assert!(!log[0].read);

        // The same event announced again (a reload's head re-announcement) is not evaluated.
        let again = h.pipeline.handle_event(room, ev, 1).await.unwrap();
        assert_eq!(again, None);
        assert_eq!(h.counts.total_unread(alice).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_rooms_old_head_seen_for_the_first_time_is_not_pushed() {
        let h = harness();
        let room = room_id!("!room:example.org");
        let ev = ruma::event_id!("$old:example.org");
        let mut old = message(ev.as_str(), "from before the restart");
        old["origin_server_ts"] = json!(now_ms() - 10 * STALE_HEAD_GRACE_MS);
        h.source
            .events
            .lock()
            .unwrap()
            .insert(ev.to_owned(), describe(old));
        assert_eq!(h.pipeline.handle_event(room, ev, 5).await.unwrap(), None);

        // The next real event in that room is evaluated.
        let next = ruma::event_id!("$new:example.org");
        h.source
            .events
            .lock()
            .unwrap()
            .insert(next.to_owned(), describe(message(next.as_str(), "news")));
        assert_eq!(
            h.pipeline.handle_event(room, next, 6).await.unwrap(),
            Some(1)
        );
    }

    #[tokio::test]
    async fn an_invite_notifies_the_invitee() {
        let h = harness();
        let room = room_id!("!room:example.org");
        let ev = ruma::event_id!("$inv:example.org");
        let invite = json!({
            "event_id": ev,
            "room_id": room,
            "type": "m.room.member",
            "sender": "@bob:example.org",
            "state_key": "@carol:example.org",
            "origin_server_ts": now_ms(),
            "content": {"membership": "invite"},
        });
        h.source
            .events
            .lock()
            .unwrap()
            .insert(ev.to_owned(), describe(invite));
        // Alice (joined) sees a member event: `.m.rule.member_event` is silent. Carol gets
        // `.m.rule.invite_for_me`.
        assert_eq!(h.pipeline.handle_event(room, ev, 1).await.unwrap(), Some(1));
        let carol = user_id!("@carol:example.org");
        assert_eq!(h.counts.total_unread(carol).await.unwrap(), 1);
        assert_eq!(
            h.counts
                .total_unread(user_id!("@alice:example.org"))
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_receipt_resets_the_room_and_marks_the_log_read() {
        let h = harness();
        let room = room_id!("!room:example.org");
        let alice = user_id!("@alice:example.org");
        for (i, id) in ["$a:example.org", "$b:example.org"].iter().enumerate() {
            let ev = EventId::parse(id).unwrap();
            h.source
                .events
                .lock()
                .unwrap()
                .insert(ev.clone(), describe(message(id, "x")));
            h.pipeline
                .handle_event(room, &ev, i64::try_from(i).unwrap())
                .await
                .unwrap();
        }
        assert_eq!(h.counts.total_unread(alice).await.unwrap(), 2);
        h.pipeline.handle_receipt(alice, room).await.unwrap();
        assert_eq!(h.counts.total_unread(alice).await.unwrap(), 0);
        let log = h.log.page(alice, None, 10, false).await.unwrap();
        assert!(log.iter().all(|e| e.read));

        // A remote user's receipt is not ours to act on.
        h.pipeline
            .handle_receipt(user_id!("@dave:remote.org"), room)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pushes_reach_the_gateway_with_the_spec_shape_and_rejections_delete_the_pusher() {
        let gateway = hs_testkit::FakePushGateway::new();
        gateway.reject_pushkey("stale");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = gateway.router();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let url = format!("http://{addr}/_matrix/push/v1/notify");

        let h = harness();
        let alice = user_id!("@alice:example.org");
        h.pushers
            .set_pusher(alice, http_pusher("fresh", &url), None)
            .await
            .unwrap();
        h.pushers
            .set_pusher(alice, http_pusher("stale", &url), None)
            .await
            .unwrap();
        let room = room_id!("!room:example.org");
        let ev = ruma::event_id!("$1:example.org");
        h.source
            .events
            .lock()
            .unwrap()
            .insert(ev.to_owned(), describe(message(ev.as_str(), "hello")));
        h.pipeline.handle_event(room, ev, 1).await.unwrap();

        // Delivery is spawned; wait for both pushes to land.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while gateway.notifications().len() < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let notifications = gateway.notifications();
        assert_eq!(notifications.len(), 2);
        let n = &notifications[0].notification;
        assert_eq!(n["type"], "m.room.message");
        assert_eq!(n["sender"], "@bob:example.org");
        assert_eq!(n["room_id"], "!room:example.org");
        assert_eq!(n["room_name"], "Test Name");
        assert_eq!(n["sender_display_name"], "Bob");
        assert_eq!(n["content"]["body"], "hello");
        assert_eq!(n["counts"]["unread"], 1);
        assert_eq!(n["id"], n["event_id"]);
        let device = &n["devices"][0];
        for key in ["app_id", "pushkey", "pushkey_ts", "data", "tweaks"] {
            assert!(device.get(key).is_some(), "device has {key}");
        }
        assert!(
            device["data"].get("url").is_none(),
            "url is not echoed to the gateway"
        );

        // The gateway rejected `stale`: that pusher is gone, the other stays.
        while h.pushers.get_pushers(alice).await.unwrap().len() > 1
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let left = h.pushers.get_pushers(alice).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].ids.pushkey, "fresh");

        // A receipt sends the zero badge.
        h.pipeline.handle_receipt(alice, room).await.unwrap();
        while gateway.notifications().len() < 3 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let badge = &gateway.notifications()[2].notification;
        assert_eq!(badge["counts"]["unread"], 0);
        assert_eq!(badge["devices"][0]["pushkey"], "fresh");
    }

    #[test]
    fn tweaks_follow_the_actions() {
        let actions: Vec<Action> = serde_json::from_value(json!([
            "notify",
            {"set_tweak": "sound", "value": "default"},
            {"set_tweak": "highlight"},
        ]))
        .unwrap();
        assert_eq!(
            tweaks_of(&actions),
            json!({"sound": "default", "highlight": true})
        );
        assert_eq!(tweaks_of(&[Action::Notify]), json!({}));
    }
}
