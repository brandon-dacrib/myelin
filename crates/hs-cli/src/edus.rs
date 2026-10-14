//! Ephemeral data across servers: the adapters between `hs-federation` (transactions, which know
//! nothing about rooms or users), `hs-user` (typing, receipts, presence) and `hs-e2e` (device
//! lists, keys and to-device messages). Here for the same reason `crate::federation_sender` is:
//! this is the one crate that depends on all three.
//!
//! - [`SenderEduOutbox`]: `hs_user::edu::EduOutbox` and `hs_e2e::federation::ToDeviceOutbox` over
//!   the federation sender -- how a local user's typing, receipt or presence change, or a
//!   to-device message for a user of another server, is queued for other servers.
//! - [`EduDispatcher`]: `hs_federation::edu::InboundEduSink` -- where `/send`'s EDUs go:
//!   typing, receipts and presence to the session hub; device-list and signing-key updates to
//!   `hs-e2e`'s copy of the remote user's device list and its device-list stream (so a local
//!   user who shares a room with the remote user is told, in `/sync`'s `device_lists.changed`,
//!   to query their keys again; `hs_e2e::federation::receive_device_list_update`); to-device
//!   messages to the recipients' device inboxes, each `message_id` once
//!   (`hs_e2e::federation::receive_direct_to_device`).
//! - [`DeviceListAnnouncer`]: follows the local device-list stream and tells the servers of
//!   everyone a changed local user shares a room with: `m.device_list_update` for each device
//!   added, changed (keys or display name) or deleted, `m.signing_key_update` when the user's
//!   master or self-signing key changed (`hs_e2e::federation::device_list_update_edus`).
//! - [`ClientRemoteKeys`]: `hs_e2e::federation::RemoteKeys` over the federation client -- how a
//!   local `/keys/query` or `/keys/claim` for a remote user reaches that user's server, and how
//!   their whole device list is fetched (`GET /user/devices/{userId}`).
//! - [`HubRoomSharing`]: `hs_e2e::federation::RoomSharing` over the session hub -- whether a
//!   remote user shares a room with a local one, which decides whether their device list is
//!   copied here.
//!
//! # Observability
//!
//! Every EDU received is logged at debug level with its origin, type and outcome, and counted in
//! `hs_federation_edus_received_total{edu_type,outcome}`; every EDU sent is logged at debug level
//! by the sender once the destination accepts its transaction, and counted in
//! `hs_federation_edus_sent_total{edu_type}` (`hs_federation::metrics`).
//!
//! # Across a restart
//!
//! To-device messages and device-list and signing-key updates are durable EDUs
//! (`hs_federation::sender::DURABLE_EDU_TYPES`, RFC 0023): the sender keeps them in its store
//! until the destination accepts them, so one queued for a server that is down survives a restart
//! of this one (Complement's `stopped_server` cases). The announcer stores how far it has read
//! the device-list stream for each federation shard ([`announcer_position`], in the sender's
//! store) and resumes from there, so a change committed while the process stopped is announced
//! at the next start; what it had announced before is not remembered, so the first change of
//! each user after a start is announced as their whole device list. Typing, receipts and
//! presence stay in memory: they describe a moment. In a cluster, an EDU for a destination whose
//! federation shard another replica owns is forwarded to that replica over the mesh
//! (`crate::edu_forward`), except the device-list announcer's: every replica follows the stream
//! and produces those for itself, and each queues them only for the destinations it sends for.
//! Each shard's place is moved on only by the replica that owns the shard, so a shard whose
//! owner died, or was down while others read on, is caught up from its own place by whoever
//! takes it next (counted in `hs_federation_device_list_catch_ups_total`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hs_cluster::{Ownership, ShardId, ShardKind, ShardLayout};
use hs_e2e::federation::{
    Announced, AnnouncedDevice, DIRECT_TO_DEVICE_EDU, InboundDeviceList, InboundToDevice,
    device_list_update_edus, local_device_list,
};
use hs_e2e::state::E2eState;
use hs_federation::edu::{Edu, InboundEduSink};
use hs_federation::metrics::{EduMetrics, EduOutcome};
use hs_federation::sender::FederationSender;
use hs_kv::KvBackend;
use hs_room::registry::RoomRegistry;
use hs_user::hub::SessionHub;
use ruma::{OwnedServerName, UserId};
use serde_json::{Value, json};

/// The session hub as `hs serve` builds it.
pub type Hub<B> = SessionHub<B, Arc<RoomRegistry<B>>>;

/// `hs_user::edu::EduOutbox` and `hs_e2e::federation::ToDeviceOutbox` over the federation
/// sender. See the module docs.
pub struct SenderEduOutbox {
    sender: Arc<FederationSender>,
}

impl SenderEduOutbox {
    /// An outbox that queues on `sender`.
    #[must_use]
    pub fn new(sender: Arc<FederationSender>) -> Self {
        Self { sender }
    }
}

impl hs_user::edu::EduOutbox for SenderEduOutbox {
    fn send_edu(
        &self,
        destinations: BTreeSet<String>,
        edu_type: &str,
        content: Value,
        coalesce_key: Option<String>,
    ) {
        self.sender
            .enqueue_edu(destinations, edu_type, content, coalesce_key);
    }
}

impl hs_e2e::federation::ToDeviceOutbox for SenderEduOutbox {
    fn send_direct_to_device(&self, destination: &str, content: Value) {
        let sender = content.get("sender").and_then(|s| s.as_str());
        let message_id = content.get("message_id").and_then(|m| m.as_str());
        tracing::debug!(
            destination,
            sender,
            message_id,
            "queueing a to-device message for another server"
        );
        // Never coalesced: every to-device message is delivered, in order; durable, kept in the
        // sender's store until the destination accepts it (RFC 0023).
        self.sender.enqueue_durable_edu(
            [destination.to_owned()],
            DIRECT_TO_DEVICE_EDU,
            content,
            None,
        );
    }
}

/// Where `/send`'s EDUs go in `hs serve`. See the module docs.
pub struct EduDispatcher<B: KvBackend> {
    hub: Arc<Hub<B>>,
    e2e: E2eState<B>,
    metrics: EduMetrics,
}

impl<B: KvBackend + 'static> EduDispatcher<B> {
    /// A dispatcher applying typing, receipts and presence to `hub`, device-list and signing-key
    /// changes to `e2e`'s stream and to-device messages to `e2e`'s inboxes, counting each EDU in
    /// `metrics`.
    #[must_use]
    pub fn new(hub: Arc<Hub<B>>, e2e: E2eState<B>, metrics: EduMetrics) -> Self {
        Self { hub, e2e, metrics }
    }

    /// Hands an `m.device_list_update` or `m.signing_key_update` to `hs-e2e`, which keeps the
    /// copy of the user's list and records the change.
    async fn device_list_changed(&self, origin: &str, edu: &Edu) -> EduOutcome {
        let result = if edu.edu_type == "m.signing_key_update" {
            hs_e2e::federation::receive_signing_key_update(&self.e2e, origin, &edu.content).await
        } else {
            hs_e2e::federation::receive_device_list_update(&self.e2e, origin, &edu.content).await
        };
        let user_id = edu.content.get("user_id").and_then(Value::as_str);
        match result {
            Ok(InboundDeviceList::Dropped(reason)) => {
                tracing::debug!(
                    origin,
                    user_id,
                    edu_type = edu.edu_type,
                    reason,
                    "dropping an EDU"
                );
                EduOutcome::Dropped
            }
            Ok(InboundDeviceList::AlreadyKnown) => EduOutcome::Duplicate,
            Ok(outcome) => {
                tracing::debug!(
                    origin,
                    user_id,
                    edu_type = edu.edu_type,
                    ?outcome,
                    "device-list EDU"
                );
                EduOutcome::Applied
            }
            Err(error) => {
                tracing::warn!(origin, user_id, %error, "could not apply a remote device-list change");
                EduOutcome::Dropped
            }
        }
    }

    /// Delivers an `m.direct_to_device` EDU's messages to this server's devices.
    async fn direct_to_device(&self, origin: &str, edu: &Edu) -> EduOutcome {
        let message_id = edu.content.get("message_id").and_then(Value::as_str);
        match hs_e2e::federation::receive_direct_to_device(&self.e2e, origin, &edu.content).await {
            Ok(InboundToDevice::Delivered { messages }) => {
                tracing::debug!(origin, message_id, messages, "delivered to-device messages");
                EduOutcome::Applied
            }
            Ok(InboundToDevice::Duplicate) => {
                tracing::debug!(
                    origin,
                    message_id,
                    "a to-device message_id already delivered; not delivering it again"
                );
                EduOutcome::Duplicate
            }
            Ok(InboundToDevice::Dropped(reason)) => {
                tracing::debug!(origin, message_id, reason, "dropping a to-device EDU");
                EduOutcome::Dropped
            }
            Err(error) => {
                tracing::warn!(origin, message_id, %error, "could not deliver a to-device EDU");
                EduOutcome::Dropped
            }
        }
    }
}

#[async_trait]
impl<B: KvBackend + 'static> InboundEduSink for EduDispatcher<B> {
    async fn receive_edu(&self, origin: &str, edu: Edu) {
        let outcome = match edu.edu_type.as_str() {
            "m.typing" | "m.receipt" | "m.presence" => {
                let applied = self
                    .hub
                    .receive_edu(origin, &edu.edu_type, &edu.content)
                    .await;
                if applied > 0 {
                    EduOutcome::Applied
                } else {
                    EduOutcome::Dropped
                }
            }
            "m.device_list_update" | "m.signing_key_update" => {
                self.device_list_changed(origin, &edu).await
            }
            DIRECT_TO_DEVICE_EDU => self.direct_to_device(origin, &edu).await,
            _ => EduOutcome::Dropped,
        };
        tracing::debug!(
            origin,
            edu_type = edu.edu_type,
            outcome = outcome.as_str(),
            "EDU received"
        );
        self.metrics.record_received(&edu.edu_type, outcome);
    }
}

/// How often [`DeviceListAnnouncer`] looks at the device-list stream without being woken: for
/// a change another replica committed (one committed in this process wakes it at once). A
/// client of another replica that uploads keys and a remote server that is told about it are
/// this far apart at most, plus the transaction.
pub const DEVICE_LIST_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Follows the local device-list stream and announces each local user's changes to the servers
/// that share a room with them. See the module docs.
///
/// Following the stream, not a hook: every device-list change -- a key upload, a device added,
/// renamed or deleted through `hs-auth`, a cross-signing key or signature -- already goes
/// through `hs-e2e`'s stream, and reading the stream catches all of them without either crate
/// learning about federation. It looks as soon as a write in this process commits
/// (`hs_e2e::store::DeviceKeyStore::subscribe_device_list_stream`), because who is told is
/// worked out when it looks: the servers sharing a room with the user then. A look a poll
/// interval late also told a server that joined in between, of a change made before it did
/// (Sytest's "Local device key changes get to remote servers", which saw the previous test's
/// user's new device first), and a remote user's sync that returned before the EDU arrived
/// missed it (Sytest's "If remote user leaves room we no longer receive device updates"). It
/// still polls every [`DEVICE_LIST_POLL_INTERVAL`] for the writes another replica commits, and
/// for federation shards it takes on. The stream names the user, not what changed, so the
/// announcer keeps what it last announced for each user to each federation shard's
/// destinations (`hs_e2e::federation::Announced`, in the sender's store:
/// `FederationSender::announced`) and sends only the difference
/// (`hs_e2e::federation::device_list_update_edus`), its `prev_id` the stream position the last
/// announcement carried. Only a user it has no record of is announced whole, with an empty
/// `prev_id`, which makes every server that receives it ask for the list back.
///
/// # Where it is in the stream
///
/// One place per federation shard ([`announcer_position`]), moved on by the replica that owns
/// the shard, once what it read is handed to the sender (whose durable store keeps it until the
/// destination accepts it). Every replica follows the same stream, and each announces only to
/// the destinations of the shards it owns ([`AnnouncerShards`]), so every destination is told
/// once. A shard this replica takes on -- at start, or from a replica that left or died -- is
/// read from its own place, and what its destinations were told is read from the records:
/// whatever the stream gained since its previous owner last read is announced to them as
/// differences, logged at `info` and counted (`hs_federation_device_list_catch_ups_total`). A
/// shard with no place yet starts where the place for the whole server stood before places were
/// kept per shard ([`ANNOUNCER_POSITION`]), or from now. In single-node mode every shard is this
/// process's, so all of them move together.
///
/// # In bounded steps, off the request threads
///
/// The stream is read [`ANNOUNCE_STEP`] positions at a time, and each shard's place is stored
/// after every step, so a process killed part-way does not start again from where it began;
/// each user's record is stored as soon as their EDUs are queued, so a step that is repeated
/// sends nothing twice. The EDUs and records are written on blocking threads: one EDU for every
/// server a user shares a room with is many store writes. Before 2026-10-14 the place moved only
/// after everything since it was announced, the records lived in memory, and the writes ran on
/// the runtime's threads: the demo, killed by its liveness probe a minute into every boot,
/// started from position 1314 every time and told some 1,400 servers every device again with no
/// `prev_id`, and each live server asked for every list back (`docs/status/06-federation.md`,
/// "The boot freeze"). Each step is timed (`hs_federation_device_list_announce_seconds`), and
/// how far the announcer is behind the stream is a gauge
/// (`hs_federation_device_list_announcer_behind`).
pub struct DeviceListAnnouncer {
    task: tokio::task::JoinHandle<()>,
}

/// How many device-list stream positions [`DeviceListAnnouncer`] reads in one step before it
/// stores its place (see "In bounded steps").
pub const ANNOUNCE_STEP: u64 = 50;

/// A step of [`DeviceListAnnouncer`] that takes this long is logged at `info`, with where it
/// got to: a slow announcer is one an operator wants to see moving.
const SLOW_ANNOUNCE_STEP: Duration = Duration::from_secs(1);

/// The name the device-list announcer stored its one stream position under in the federation
/// sender's store (`FederationSender::store_position`) before positions were kept per federation
/// shard ([`announcer_position`]): read once, as the place of a shard that has none.
pub const ANNOUNCER_POSITION: &str = "device_list_announcer";

/// The name the device-list announcer stores federation shard `shard`'s place in the
/// device-list stream under, in the federation sender's store.
#[must_use]
pub fn announcer_position(shard: u32) -> String {
    format!("{ANNOUNCER_POSITION}/federation/{shard}")
}

/// Which federation shards this replica sends for, and which shard a destination is in: what
/// [`DeviceListAnnouncer`] keeps its places by. The same ownership and layout as the sender's
/// gate (`crate::federation_sender::ShardGate`).
#[derive(Clone)]
pub struct AnnouncerShards {
    ownership: Arc<dyn Ownership>,
    layout: ShardLayout,
}

impl AnnouncerShards {
    /// Shards of `layout`, owned as `ownership` says.
    #[must_use]
    pub fn new(ownership: Arc<dyn Ownership>, layout: ShardLayout) -> Self {
        Self { ownership, layout }
    }

    /// The federation shards this replica owns now.
    fn owned(&self) -> BTreeSet<u32> {
        (0..self.layout.federation)
            .filter(|index| {
                self.ownership
                    .is_mine(ShardId::new(ShardKind::Federation, *index))
            })
            .collect()
    }

    /// `destination`'s federation shard.
    fn shard_of(&self, destination: &str) -> u32 {
        self.layout.federation_shard(destination).index
    }
}

impl DeviceListAnnouncer {
    /// Starts following `e2e`'s stream, announcing through `sender` to the servers `hub` says
    /// share a room with each changed user of `own_server`, for the federation shards `shards`
    /// says this replica owns (see "Where it is in the stream").
    #[must_use]
    pub fn start<B: KvBackend + 'static>(
        hub: Arc<Hub<B>>,
        e2e: E2eState<B>,
        sender: Arc<FederationSender>,
        own_server: OwnedServerName,
        shards: AnnouncerShards,
    ) -> Self {
        let task = tokio::spawn(async move {
            // The place for the whole server, from before places were kept per shard.
            let legacy = off_runtime(&sender, |sender| {
                sender.stored_position(ANNOUNCER_POSITION).ok().flatten()
            })
            .await
            .flatten();
            // Each owned shard's place, as far as this replica has read.
            let mut places: BTreeMap<u32, u64> = BTreeMap::new();
            let mut interval = tokio::time::interval(DEVICE_LIST_POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Woken by this process's own writes as they commit; the poll stays for the writes
            // of other replicas on a shared backend, for shards taken on, and for a store that
            // cannot say.
            let mut woken = e2e.store.subscribe_device_list_stream();
            let mut behind = false;
            loop {
                // A step that left the stream unread goes on at once.
                if !behind {
                    next_look(&mut interval, &mut woken).await;
                }
                behind = false;
                let now = match e2e.store.current_stream_pos().await {
                    Ok(pos) => pos,
                    Err(error) => {
                        tracing::warn!(%error, "cannot read the device-list stream");
                        continue;
                    }
                };
                let owned = shards.owned();
                places.retain(|shard, _| owned.contains(shard));
                take_on(&sender, &owned, &mut places, legacy, now).await;
                for (from, group) in group_by_place(&places) {
                    let Some(to) = step_end(from, now) else {
                        continue;
                    };
                    let started = std::time::Instant::now();
                    let Some(users) = announce_step(
                        &hub,
                        &e2e,
                        &sender,
                        &shards,
                        &own_server,
                        (from, to),
                        &group,
                    )
                    .await
                    else {
                        // Not read (logged): the same step again at the next look; the users'
                        // records keep what was already sent from being sent again.
                        continue;
                    };
                    // Handed over (the EDUs are durable in the sender's store): each shard still
                    // owned begins here next time, on this replica or the next owner.
                    let still = shards.owned();
                    let moved: Vec<u32> = group
                        .iter()
                        .copied()
                        .filter(|shard| still.contains(shard))
                        .collect();
                    for shard in &moved {
                        places.insert(*shard, to);
                    }
                    let writes: Vec<(String, u64)> = moved
                        .iter()
                        .map(|shard| (announcer_position(*shard), to))
                        .collect();
                    match off_runtime(&sender, move |sender| sender.store_positions(&writes)).await
                    {
                        Some(Ok(())) => {}
                        Some(Err(error)) => tracing::warn!(
                            %error,
                            "cannot store where the device-list announcer got to"
                        ),
                        None => tracing::warn!(
                            "the device-list announcer's place was not stored (its thread did not finish)"
                        ),
                    }
                    let took = started.elapsed();
                    hs_federation::metrics::record_device_list_announce_step(
                        took.as_secs_f64(),
                        users,
                    );
                    if took >= SLOW_ANNOUNCE_STEP {
                        tracing::info!(
                            from,
                            to,
                            stream = now,
                            users,
                            shards = moved.len(),
                            took_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
                            "the device-list announcer stored its place after a slow step"
                        );
                    }
                    if to < now {
                        behind = true;
                    }
                }
                let oldest = places.values().copied().min().unwrap_or(now);
                hs_federation::metrics::set_device_list_announcer_behind(
                    now.saturating_sub(oldest),
                );
            }
        });
        Self { task }
    }

    /// Stops following the stream.
    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for DeviceListAnnouncer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Runs `work` with `sender` on a blocking thread: the sender's store calls wait for the
/// backend, and the announcer's are many. `None` when the thread did not finish it (a panic,
/// or the runtime shutting down).
async fn off_runtime<T: Send + 'static>(
    sender: &Arc<FederationSender>,
    work: impl FnOnce(&FederationSender) -> T + Send + 'static,
) -> Option<T> {
    let sender = sender.clone();
    tokio::task::spawn_blocking(move || work(&sender))
        .await
        .ok()
}

/// Gives every shard in `owned` that has no place in `places` yet (taken on at start, or from a
/// replica that left or died) its stored place -- or the old whole-server one, `legacy`, or
/// `now` with neither, never past `now` (a store reset under it) -- and logs and counts those
/// whose place is behind: their destinations are told what they missed, from the records.
async fn take_on(
    sender: &Arc<FederationSender>,
    owned: &BTreeSet<u32>,
    places: &mut BTreeMap<u32, u64>,
    legacy: Option<u64>,
    now: u64,
) {
    let taken: Vec<u32> = owned
        .iter()
        .copied()
        .filter(|shard| !places.contains_key(shard))
        .collect();
    if taken.is_empty() {
        return;
    }
    let names = taken.clone();
    let stored = off_runtime(sender, move |sender| {
        names
            .into_iter()
            .map(|shard| {
                let place = match sender.stored_position(&announcer_position(shard)) {
                    Ok(stored) => stored,
                    Err(error) => {
                        tracing::warn!(shard, %error, "cannot read where the device-list announcer got to for a federation shard");
                        None
                    }
                };
                (shard, place)
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_else(|| taken.iter().map(|shard| (*shard, None)).collect());
    let mut behind: BTreeMap<u64, BTreeSet<u32>> = BTreeMap::new();
    for (shard, place) in stored {
        let from = place.or(legacy).unwrap_or(now).min(now);
        places.insert(shard, from);
        if from < now {
            behind.entry(from).or_default().insert(shard);
        }
    }
    for (from, shards) in behind {
        hs_federation::metrics::record_device_list_catch_up();
        tracing::info!(
            from,
            to = now,
            shards = ?shards,
            "announcing the device-list changes federation shards taken on were not told"
        );
    }
}

/// The shards of `places` grouped by their place, so each group reads one stretch of the
/// stream once.
fn group_by_place(places: &BTreeMap<u32, u64>) -> BTreeMap<u64, BTreeSet<u32>> {
    let mut groups: BTreeMap<u64, BTreeSet<u32>> = BTreeMap::new();
    for (shard, place) in places {
        groups.entry(*place).or_default().insert(*shard);
    }
    groups
}

/// Where a step from `from` ends when the stream is at `now`: at most [`ANNOUNCE_STEP`]
/// positions on, never past `now`; `None` when there is nothing to read.
fn step_end(from: u64, now: u64) -> Option<u64> {
    (from < now).then(|| from.saturating_add(ANNOUNCE_STEP).min(now))
}

/// Announces the changes of `(from, to]` of every local user to the destinations of `group`'s
/// federation shards, each as the difference from what those shards were last told (the
/// records). The number of local users announced, or `None` when the stream or a user's
/// devices could not be read (logged): the step is then made again.
#[allow(clippy::too_many_arguments)]
async fn announce_step<B: KvBackend + 'static>(
    hub: &Hub<B>,
    e2e: &E2eState<B>,
    sender: &Arc<FederationSender>,
    shards: &AnnouncerShards,
    own_server: &OwnedServerName,
    (from, to): (u64, u64),
    group: &BTreeSet<u32>,
) -> Option<u64> {
    let changed = match e2e.store.changed_users_since(from, Some(to)).await {
        Ok(users) => users,
        Err(error) => {
            tracing::warn!(%error, "cannot read the device-list changes");
            return None;
        }
    };
    let mut announced = 0u64;
    for user_id in changed {
        if user_id.server_name() != own_server {
            continue;
        }
        announce(hub, e2e, sender, shards, &user_id, group).await?;
        announced += 1;
    }
    Some(announced)
}

/// Waits for the announcer's next look at the stream: a write committed in this process
/// (`woken`), or the poll interval for anything else. A closed `woken` falls back to the poll.
async fn next_look(
    interval: &mut tokio::time::Interval,
    woken: &mut Option<tokio::sync::watch::Receiver<u64>>,
) {
    let Some(rx) = woken.as_mut() else {
        interval.tick().await;
        return;
    };
    tokio::select! {
        _ = interval.tick() => {}
        changed = rx.changed() => {
            if changed.is_err() {
                *woken = None;
            } else {
                rx.borrow_and_update();
            }
        }
    }
}

/// `announced` as the sender's store keeps it (`FederationSender::set_announced`).
fn announced_to_json(announced: &Announced) -> Value {
    let devices: serde_json::Map<String, Value> = announced
        .devices
        .iter()
        .map(|(device_id, device)| {
            (
                device_id.clone(),
                json!({"keys": device.keys, "display_name": device.display_name}),
            )
        })
        .collect();
    json!({
        "stream_id": announced.stream_id,
        "devices": devices,
        "master": announced.master,
        "self_signing": announced.self_signing,
    })
}

/// [`announced_to_json`] read back; `None` for a record that is not one.
fn announced_from_json(record: &Value) -> Option<Announced> {
    let devices = record
        .get("devices")?
        .as_object()?
        .iter()
        .map(|(device_id, device)| {
            (
                device_id.clone(),
                AnnouncedDevice {
                    keys: device.get("keys").filter(|keys| !keys.is_null()).cloned(),
                    display_name: device
                        .get("display_name")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            )
        })
        .collect();
    let field = |name: &str| record.get(name).filter(|value| !value.is_null()).cloned();
    Some(Announced {
        stream_id: record.get("stream_id").and_then(Value::as_u64),
        devices,
        master: field("master"),
        self_signing: field("self_signing"),
    })
}

/// Sends what changed for `user_id` to the destinations of `group`'s shards that share a joined
/// room with them -- to each set of shards the difference from what it was last told (its
/// record in the sender's store) -- and then records what they were all told now. `None`
/// when something could not be read or recorded (logged); nothing is recorded then.
async fn announce<B: KvBackend + 'static>(
    hub: &Hub<B>,
    e2e: &E2eState<B>,
    sender: &Arc<FederationSender>,
    shards: &AnnouncerShards,
    user_id: &UserId,
    group: &BTreeSet<u32>,
) -> Option<()> {
    let now = match local_device_list(e2e, user_id).await {
        Ok(now) => now,
        Err(error) => {
            tracing::warn!(%user_id, %error, "cannot read a user's devices to announce them");
            return None;
        }
    };
    let user = user_id.to_string();
    let records = match off_runtime(sender, move |sender| sender.announced(&user)).await {
        Some(Ok(records)) => records,
        Some(Err(error)) => {
            tracing::warn!(%user_id, %error, "cannot read what was announced of a user's devices");
            return None;
        }
        None => return None,
    };
    // The shards by what they were told: in single-node mode, all of them at once.
    let mut told: BTreeMap<String, (Option<Announced>, BTreeSet<u32>)> = BTreeMap::new();
    for shard in group {
        let record = records.get(shard);
        let key = record.map(Value::to_string).unwrap_or_default();
        told.entry(key)
            .or_insert_with(|| (record.and_then(announced_from_json), BTreeSet::new()))
            .1
            .insert(*shard);
    }
    let mut audience: Option<BTreeSet<String>> = None;
    let mut after = None;
    for (before, partition) in told.into_values() {
        let (edus, announced) = device_list_update_edus(user_id, before.as_ref(), now.clone());
        after.get_or_insert(announced);
        if edus.is_empty() {
            tracing::debug!(%user_id, "a device-list change with nothing another server is told about");
            continue;
        }
        let servers = match &audience {
            Some(servers) => servers,
            None => audience.insert(servers_sharing_a_room(hub, user_id).await?),
        };
        let destinations: Vec<String> = servers
            .iter()
            .filter(|server| partition.contains(&shards.shard_of(server)))
            .cloned()
            .collect();
        if destinations.is_empty() {
            // Said, because a server that should have been told and was not is otherwise silent.
            tracing::debug!(
                %user_id,
                servers = servers.len(),
                "a device-list change with no other server sharing a room with the user"
            );
            continue;
        }
        for (edu_type, content) in edus {
            let coalesce_key = match edu_type {
                "m.signing_key_update" => format!("signing {user_id}"),
                _ => format!(
                    "device {user_id} {}",
                    content["device_id"].as_str().unwrap_or_default()
                ),
            };
            tracing::debug!(
                %user_id,
                edu_type,
                destinations = destinations.len(),
                "announcing a key change to other servers"
            );
            // Local only: every replica follows this stream, so the owner of each destination
            // announces to it already (`crate::edu_forward`). Durable, as a device-list or
            // signing-key update is (`hs_federation::sender::DURABLE_EDU_TYPES`); written on a
            // blocking thread, a batch of destinations at a time.
            let destinations = destinations.clone();
            off_runtime(sender, move |sender| {
                sender.enqueue_edu_local(destinations, edu_type, content, Some(coalesce_key));
            })
            .await?;
        }
    }
    let after = after?;
    let record = announced_to_json(&after);
    let shards: Vec<u32> = group.iter().copied().collect();
    let user = user_id.to_string();
    match off_runtime(sender, move |sender| {
        sender.set_announced(&user, &shards, &record)
    })
    .await
    {
        Some(Ok(())) => Some(()),
        Some(Err(error)) => {
            // The EDUs are queued; without the record the next look tells the same again.
            tracing::warn!(%user_id, %error, "cannot record what was announced of a user's devices");
            None
        }
        None => None,
    }
}

/// The servers, other than the user's own, of everyone `user_id` shares a joined room with.
async fn servers_sharing_a_room<B: KvBackend + 'static>(
    hub: &Hub<B>,
    user_id: &UserId,
) -> Option<BTreeSet<String>> {
    match hub.users_sharing_room_with(user_id).await {
        Ok(users) => Some(
            users
                .iter()
                .map(|u| u.server_name().to_string())
                .filter(|server| server != user_id.server_name().as_str())
                .collect(),
        ),
        Err(error) => {
            tracing::warn!(%user_id, %error, "cannot work out who to tell about a device-list change");
            None
        }
    }
}

/// `hs_e2e::federation::RemoteKeys` over the federation client. See the module docs.
pub struct ClientRemoteKeys {
    client: Arc<hs_federation::client::FederationClient>,
}

impl ClientRemoteKeys {
    /// Remote key access through `client`.
    #[must_use]
    pub fn new(client: Arc<hs_federation::client::FederationClient>) -> Self {
        Self { client }
    }

    async fn post(&self, server: &str, path: &str, body: Value) -> Result<Value, String> {
        match self.client.send(server, "POST", path, Some(&body)).await {
            Ok(response) if response.status == 200 => Ok(response.body),
            Ok(response) => Err(format!("HTTP {}: {}", response.status, response.body)),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[async_trait]
impl hs_e2e::federation::RemoteKeys for ClientRemoteKeys {
    async fn query(&self, server: &str, device_keys: Value) -> Result<Value, String> {
        self.post(
            server,
            "/_matrix/federation/v1/user/keys/query",
            json!({"device_keys": device_keys}),
        )
        .await
    }

    async fn claim(&self, server: &str, one_time_keys: Value) -> Result<Value, String> {
        self.post(
            server,
            "/_matrix/federation/v1/user/keys/claim",
            json!({"one_time_keys": one_time_keys}),
        )
        .await
    }

    async fn devices(&self, server: &str, user_id: &str) -> Result<Value, String> {
        // `@` and `:` are path characters; nothing in a user id needs escaping.
        let path = format!("/_matrix/federation/v1/user/devices/{user_id}");
        match self.client.send(server, "GET", &path, None).await {
            Ok(response) if response.status == 200 => Ok(response.body),
            Ok(response) => Err(format!("HTTP {}: {}", response.status, response.body)),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// `hs_e2e::federation::RoomSharing` over the session hub's membership records. See the module
/// docs.
pub struct HubRoomSharing<B: KvBackend> {
    hub: Arc<Hub<B>>,
    own_server: OwnedServerName,
}

impl<B: KvBackend> HubRoomSharing<B> {
    /// Answers from `hub` for users of `own_server`.
    #[must_use]
    pub fn new(hub: Arc<Hub<B>>, own_server: OwnedServerName) -> Self {
        Self { hub, own_server }
    }
}

#[async_trait]
impl<B: KvBackend + 'static> hs_e2e::federation::RoomSharing for HubRoomSharing<B> {
    async fn shares_a_room_with_a_local_user(&self, user_id: &UserId) -> bool {
        match self.hub.users_sharing_room_with(user_id).await {
            Ok(users) => users.iter().any(|u| u.server_name() == self.own_server),
            Err(error) => {
                tracing::warn!(%user_id, %error, "cannot work out whether a remote user shares a room here");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shards at one place are read together; a step reads at most `ANNOUNCE_STEP` positions
    /// and never past the stream, and there is no step when the place is the stream's end.
    #[test]
    fn shards_at_one_place_are_read_together_a_step_at_a_time() {
        let groups = group_by_place(&BTreeMap::from([(0, 5), (1, 10), (2, 5), (3, 10)]));
        assert_eq!(
            groups,
            BTreeMap::from([(5, BTreeSet::from([0, 2])), (10, BTreeSet::from([1, 3]))])
        );
        assert_eq!(step_end(1314, 1386), Some(1314 + ANNOUNCE_STEP));
        assert_eq!(step_end(1380, 1386), Some(1386));
        assert_eq!(step_end(1386, 1386), None);
        assert_eq!(step_end(1400, 1386), None);
    }

    /// A shard taken on starts at its stored place, at the old whole-server one without, at
    /// now with neither, never past now; and only one behind is a catch-up.
    #[tokio::test]
    async fn a_shard_taken_on_starts_at_its_stored_place_or_the_old_one_or_now() {
        use hs_federation::discovery::{
            AddrResolver, SrvResolver, WellKnownFetcher, WellKnownOutcome,
        };
        /// Resolves nothing: this sender never sends.
        struct Nowhere;
        #[async_trait]
        impl WellKnownFetcher for Nowhere {
            async fn fetch(&self, _hostname: &str) -> WellKnownOutcome {
                WellKnownOutcome::Absent {
                    cache_for: Duration::from_secs(60),
                }
            }
        }
        #[async_trait]
        impl SrvResolver for Nowhere {
            async fn lookup_srv(&self, _service: &str, _hostname: &str) -> Vec<(String, u16)> {
                Vec::new()
            }
        }
        #[async_trait]
        impl AddrResolver for Nowhere {
            async fn resolve_addr(&self, _hostname: &str) -> Vec<std::net::IpAddr> {
                Vec::new()
            }
        }
        let client = Arc::new(hs_federation::client::FederationClient::new(
            "a.example",
            hs_model::signing::SigningKeyPair::generate("t"),
            hs_federation::client::ClientConfig::default(),
            Arc::new(hs_federation::destination_store::InMemoryDestinationStore::new()),
            Arc::new(Nowhere),
            Arc::new(Nowhere),
            Arc::new(Nowhere),
        ));
        let sender = Arc::new(FederationSender::new(client, "a.example"));
        sender.store_position(&announcer_position(0), 5).unwrap();
        sender.store_position(&announcer_position(2), 50).unwrap();
        let catch_ups = hs_federation::metrics::device_list_catch_ups();

        let mut places = BTreeMap::new();
        take_on(&sender, &BTreeSet::from([0, 1, 2]), &mut places, None, 10).await;
        assert_eq!(places, BTreeMap::from([(0, 5), (1, 10), (2, 10)]));
        assert_eq!(
            hs_federation::metrics::device_list_catch_ups(),
            catch_ups + 1,
            "shard 0 alone was behind"
        );

        let mut places = BTreeMap::from([(0, 8)]);
        take_on(&sender, &BTreeSet::from([0, 3]), &mut places, Some(2), 12).await;
        assert_eq!(
            places,
            BTreeMap::from([(0, 8), (3, 2)]),
            "shard 0 kept its place"
        );
    }

    /// What was announced is kept as JSON in the sender's store and read back the same, devices,
    /// names, cross-signing keys and stream position alike; and a record that is not one is
    /// none, so the user is announced whole.
    #[test]
    fn an_announcement_record_reads_back_as_it_was_written() {
        let announced = Announced {
            stream_id: Some(42),
            devices: BTreeMap::from([
                (
                    "D1".to_owned(),
                    AnnouncedDevice {
                        keys: Some(json!({"k": 1})),
                        display_name: Some("phone".to_owned()),
                    },
                ),
                (
                    "D2".to_owned(),
                    AnnouncedDevice {
                        keys: None,
                        display_name: None,
                    },
                ),
            ]),
            master: Some(json!({"m": 1})),
            self_signing: None,
        };
        let record = announced_to_json(&announced);
        assert_eq!(announced_from_json(&record), Some(announced));
        assert_eq!(announced_from_json(&json!("not a record")), None);
    }

    /// Each federation shard's place has its own name, beside the old whole-server one.
    #[test]
    fn each_shard_has_its_own_place() {
        assert_eq!(announcer_position(7), "device_list_announcer/federation/7");
        assert_ne!(announcer_position(0), ANNOUNCER_POSITION);
    }
    use hs_e2e::federation::LocalDeviceList;

    fn list(devices: &[(&str, Value)], master: Option<Value>, stream_id: u64) -> LocalDeviceList {
        LocalDeviceList {
            devices: devices
                .iter()
                .map(|(d, k)| {
                    (
                        (*d).to_owned(),
                        AnnouncedDevice {
                            keys: Some(k.clone()),
                            display_name: None,
                        },
                    )
                })
                .collect(),
            master,
            self_signing: None,
            stream_id,
        }
    }

    fn types(edus: &[(&'static str, Value)]) -> Vec<&'static str> {
        edus.iter().map(|(t, _)| *t).collect()
    }

    /// The announcer's contract with `hs-e2e`'s diff: the first change seen for a user
    /// announces everything, and the next announcement names the first in `prev_id`.
    #[test]
    fn the_first_change_seen_for_a_user_announces_everything_and_the_next_follows_it() {
        let user = ruma::user_id!("@alice:a.example");
        let (edus, after) = device_list_update_edus(
            user,
            None,
            list(&[("D1", json!({"k": 1}))], Some(json!({"m": 1})), 7),
        );
        assert_eq!(
            types(&edus),
            ["m.device_list_update", "m.signing_key_update"]
        );
        assert_eq!(edus[0].1["prev_id"], json!([]));
        assert_eq!(edus[0].1["stream_id"], 7);
        assert_eq!(edus[1].1["master_key"], json!({"m": 1}));
        let (edus, _) = device_list_update_edus(
            user,
            Some(&after),
            list(&[("D1", json!({"k": 2}))], Some(json!({"m": 1})), 9),
        );
        assert_eq!(types(&edus), ["m.device_list_update"]);
        assert_eq!(edus[0].1["prev_id"], json!([7]));
        assert_eq!(edus[0].1["stream_id"], 9);
    }

    /// The announcer looks as soon as a write commits, not at its next poll: who is told is
    /// worked out when it looks, and a late look tells a server that joined in between (see
    /// [`DeviceListAnnouncer`]). Time is paused, so a look that waited for the interval would
    /// show it.
    #[tokio::test(start_paused = true)]
    async fn a_committed_write_wakes_the_announcer_before_its_poll_and_a_closed_watch_falls_back() {
        let mut interval = tokio::time::interval(DEVICE_LIST_POLL_INTERVAL);
        interval.tick().await;
        let (tx, rx) = tokio::sync::watch::channel(0_u64);
        let mut woken = Some(rx);
        let started = tokio::time::Instant::now();
        tx.send_replace(5);
        next_look(&mut interval, &mut woken).await;
        assert_eq!(started.elapsed(), Duration::ZERO, "woken at once");

        drop(tx);
        next_look(&mut interval, &mut woken).await;
        assert!(woken.is_none(), "a closed watch is let go");
        next_look(&mut interval, &mut woken).await;
        assert!(
            started.elapsed() >= DEVICE_LIST_POLL_INTERVAL,
            "then the poll paces it"
        );
    }
}
